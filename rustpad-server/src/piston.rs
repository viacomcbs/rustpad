//! Client for the Piston code execution engine.

use std::cmp::Ordering;
use std::fmt;
use std::time::Duration;

use anyhow::Context;
use log::warn;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Largest stdout or stderr kept from one stage, in bytes.
pub const MAX_STREAM_BYTES: usize = 64 * 1024;

/// Largest Piston response accepted, in bytes.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Time allowed to open a connection to Piston.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Time allowed for a whole Piston request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Limits sent with every execution. Piston's own config caps these.
const RUN_TIMEOUT_MS: u64 = 3000;
const COMPILE_TIMEOUT_MS: u64 = 10000;
const MEMORY_LIMIT_BYTES: i64 = 256 * 1024 * 1024;

/// Monaco language IDs whose Piston language name differs.
const OVERRIDES: &[(&str, &str)] = &[("cpp", "c++"), ("shell", "bash")];

/// A runtime installed in Piston, as returned by `GET /api/v2/runtimes`.
#[derive(Clone, Debug, Deserialize)]
pub struct PistonRuntime {
    /// Piston language name, such as `python` or `c++`.
    pub language: String,
    /// Installed version, usually semver.
    pub version: String,
    /// Other names Piston accepts for this language.
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// A Monaco language this server can run, sent to clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Runtime {
    /// Monaco language ID, as used by the editor.
    pub language: String,
    /// Piston language name used to run it.
    pub runtime: String,
    /// Piston runtime version.
    pub version: String,
}

/// Output of one stage (compile or run) of an execution.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunStage {
    /// Standard output.
    pub stdout: String,
    /// Standard error.
    pub stderr: String,
    /// Exit code, or `None` if the process was killed by a signal.
    pub code: Option<i32>,
    /// Signal that killed the process, such as `SIGKILL`.
    pub signal: Option<String>,
    /// Set when stdout or stderr was cut to `MAX_STREAM_BYTES`.
    pub truncated: bool,
}

impl RunStage {
    /// Cuts stdout and stderr to `MAX_STREAM_BYTES`, setting `truncated`.
    fn truncate(&mut self) {
        let stdout = truncate_utf8(&mut self.stdout, MAX_STREAM_BYTES);
        let stderr = truncate_utf8(&mut self.stderr, MAX_STREAM_BYTES);
        self.truncated |= stdout || stderr;
    }
}

/// Result of `POST /api/v2/execute`. `run` is absent when compilation fails.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ExecOutput {
    /// Compile stage, for compiled languages.
    pub compile: Option<RunStage>,
    /// Run stage, absent if compilation failed.
    pub run: Option<RunStage>,
}

/// Why a Piston call failed. `Display` gives a message safe to show users.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunError {
    /// Piston could not be reached, or answered with an error status.
    Unavailable,
    /// Piston did not answer in time.
    TimedOut,
    /// Piston's answer was too large or not valid JSON.
    InvalidResponse,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RunError::Unavailable => "Code runner is unavailable",
            RunError::TimedOut => "Code runner timed out",
            RunError::InvalidResponse => "Code runner returned an invalid response",
        })
    }
}

impl std::error::Error for RunError {}

/// HTTP client for the two Piston endpoints Rustpad uses.
#[derive(Clone, Debug)]
pub struct PistonClient {
    http: reqwest::Client,
    base: String,
}

#[derive(Serialize)]
struct ExecRequest<'a> {
    language: &'a str,
    version: &'a str,
    files: [SourceFile<'a>; 1],
    compile_timeout: u64,
    run_timeout: u64,
    compile_memory_limit: i64,
    run_memory_limit: i64,
}

#[derive(Serialize)]
struct SourceFile<'a> {
    content: &'a str,
}

impl PistonClient {
    /// Creates a client with the default timeouts.
    pub fn new(base_url: &str) -> anyhow::Result<Self> {
        Self::with_timeouts(base_url, CONNECT_TIMEOUT, REQUEST_TIMEOUT)
    }

    /// Creates a client with custom timeouts.
    pub fn with_timeouts(base_url: &str, connect: Duration, total: Duration) -> anyhow::Result<Self> {
        let base = reqwest::Url::parse(base_url).context("invalid Piston URL")?;
        let http = reqwest::Client::builder()
            .connect_timeout(connect)
            .timeout(total)
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self {
            http,
            base: base.as_str().trim_end_matches('/').to_owned(),
        })
    }

    /// Lists the runtimes installed in Piston.
    pub async fn runtimes(&self) -> Result<Vec<PistonRuntime>, RunError> {
        let response = self
            .http
            .get(format!("{}/api/v2/runtimes", self.base))
            .send()
            .await
            .map_err(from_reqwest)?;
        parse(response).await
    }

    /// Runs `source` as a single file with the given runtime.
    pub async fn execute(
        &self,
        language: &str,
        version: &str,
        source: &str,
    ) -> Result<ExecOutput, RunError> {
        let request = ExecRequest {
            language,
            version,
            files: [SourceFile { content: source }],
            compile_timeout: COMPILE_TIMEOUT_MS,
            run_timeout: RUN_TIMEOUT_MS,
            compile_memory_limit: MEMORY_LIMIT_BYTES,
            run_memory_limit: MEMORY_LIMIT_BYTES,
        };
        let response = self
            .http
            .post(format!("{}/api/v2/execute", self.base))
            .json(&request)
            .send()
            .await
            .map_err(from_reqwest)?;
        let mut output: ExecOutput = parse(response).await?;
        for stage in output.compile.iter_mut().chain(output.run.iter_mut()) {
            stage.truncate();
        }
        Ok(output)
    }
}

/// Reads a capped response body and decodes it as JSON.
async fn parse<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T, RunError> {
    let status = response.status();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(from_reqwest)? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            warn!("Piston response exceeded {MAX_BODY_BYTES} bytes");
            return Err(RunError::InvalidResponse);
        }
        body.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        warn!("Piston returned {status}: {}", String::from_utf8_lossy(&body));
        return Err(RunError::Unavailable);
    }
    serde_json::from_slice(&body).map_err(|e| {
        warn!("invalid Piston response: {e}");
        RunError::InvalidResponse
    })
}

/// Logs a transport error and maps it to a user-facing one.
fn from_reqwest(e: reqwest::Error) -> RunError {
    warn!("Piston request failed: {e}");
    if e.is_timeout() {
        RunError::TimedOut
    } else {
        RunError::Unavailable
    }
}

/// Returns the Monaco language IDs offered by the editor.
pub fn monaco_languages() -> Vec<String> {
    serde_json::from_str(include_str!("../../src/languages.json"))
        .expect("src/languages.json is a JSON array of strings")
}

/// Maps each Monaco language to the newest matching Piston runtime.
pub fn resolve(monaco: &[String], available: &[PistonRuntime]) -> Vec<Runtime> {
    monaco
        .iter()
        .filter_map(|language| {
            let name = OVERRIDES
                .iter()
                .find(|(id, _)| id == language)
                .map_or(language.as_str(), |(_, piston)| piston);
            available
                .iter()
                .filter(|r| r.language == name || r.aliases.iter().any(|a| a == name))
                .max_by(|a, b| compare_versions(&a.version, &b.version))
                .map(|r| Runtime {
                    language: language.clone(),
                    runtime: r.language.clone(),
                    version: r.version.clone(),
                })
        })
        .collect()
}

/// Orders versions by semver, ranking unparseable versions lowest.
fn compare_versions(a: &str, b: &str) -> Ordering {
    match (semver::Version::parse(a), semver::Version::parse(b)) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        (Ok(_), Err(_)) => Ordering::Greater,
        (Err(_), Ok(_)) => Ordering::Less,
        (Err(_), Err(_)) => a.cmp(b),
    }
}

/// Cuts `text` to at most `max` bytes on a character boundary.
/// Returns whether anything was removed.
fn truncate_utf8(text: &mut String, max: usize) -> bool {
    if text.len() <= max {
        return false;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn piston(language: &str, version: &str, aliases: &[&str]) -> PistonRuntime {
        PistonRuntime {
            language: language.into(),
            version: version.into(),
            aliases: aliases.iter().map(|&a| a.into()).collect(),
        }
    }

    fn langs(names: &[&str]) -> Vec<String> {
        names.iter().map(|&n| n.into()).collect()
    }

    #[test]
    fn resolve_matches_language_name() {
        let got = resolve(&langs(&["python"]), &[piston("python", "3.12.0", &[])]);
        assert_eq!(
            got,
            vec![Runtime {
                language: "python".into(),
                runtime: "python".into(),
                version: "3.12.0".into(),
            }]
        );
    }

    #[test]
    fn resolve_matches_alias() {
        let got = resolve(&langs(&["py"]), &[piston("python", "3.12.0", &["py"])]);
        assert_eq!(got[0].runtime, "python");
        assert_eq!(got[0].language, "py");
    }

    #[test]
    fn resolve_uses_override_table() {
        let available = [piston("c++", "10.2.0", &["g++"]), piston("bash", "5.2.0", &["sh"])];
        let got = resolve(&langs(&["cpp", "shell"]), &available);
        assert_eq!(got[0].language, "cpp");
        assert_eq!(got[0].runtime, "c++");
        assert_eq!(got[1].language, "shell");
        assert_eq!(got[1].runtime, "bash");
    }

    #[test]
    fn resolve_picks_newest_semver() {
        let available = [
            piston("python", "3.9.4", &[]),
            piston("python", "3.12.0", &[]),
            piston("python", "3.10.0", &[]),
        ];
        let got = resolve(&langs(&["python"]), &available);
        assert_eq!(got[0].version, "3.12.0");
    }

    #[test]
    fn resolve_prefers_semver_over_unparseable() {
        let available = [piston("go", "weird", &[]), piston("go", "1.16.2", &[])];
        let got = resolve(&langs(&["go"]), &available);
        assert_eq!(got[0].version, "1.16.2");
    }

    #[test]
    fn resolve_skips_languages_without_runtime() {
        let got = resolve(&langs(&["markdown", "python"]), &[piston("python", "3.12.0", &[])]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].language, "python");
    }

    #[test]
    fn monaco_languages_loads_editor_list() {
        let langs = monaco_languages();
        for name in ["python", "cpp", "shell", "plaintext", "rust"] {
            assert!(langs.iter().any(|l| l == name), "missing {name}");
        }
    }

    #[test]
    fn truncate_leaves_short_text() {
        let mut text = String::from("hello");
        assert!(!truncate_utf8(&mut text, 10));
        assert_eq!(text, "hello");
    }

    #[test]
    fn truncate_cuts_long_text() {
        let mut text = "a".repeat(20);
        assert!(truncate_utf8(&mut text, 10));
        assert_eq!(text, "a".repeat(10));
    }

    #[test]
    fn truncate_respects_char_boundary() {
        // "é" is two bytes; a cut at byte 10 would split it.
        let mut text = format!("{}é", "a".repeat(9));
        assert!(truncate_utf8(&mut text, 10));
        assert_eq!(text, "a".repeat(9));
    }

    #[test]
    fn stage_truncate_sets_flag() {
        let mut stage = RunStage {
            stdout: "a".repeat(MAX_STREAM_BYTES + 1),
            stderr: "short".into(),
            ..RunStage::default()
        };
        stage.truncate();
        assert!(stage.truncated);
        assert_eq!(stage.stdout.len(), MAX_STREAM_BYTES);
        assert_eq!(stage.stderr, "short");
    }

    #[test]
    fn stage_deserializes_piston_shape() {
        let stage: RunStage = serde_json::from_str(
            r#"{"stdout":"hi\n","stderr":"","output":"hi\n","code":0,"signal":null}"#,
        )
        .unwrap();
        assert_eq!(stage.stdout, "hi\n");
        assert_eq!(stage.code, Some(0));
        assert!(!stage.truncated);
    }
}
