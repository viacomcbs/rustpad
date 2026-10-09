# Code execution with Piston – implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let everyone in a pad run the pad's code through a self-hosted Piston and see the same output.

**Architecture:** The Rust server holds a thin Piston HTTP client. A background task keeps a list of runnable languages in a `tokio::sync::watch` channel. A new `Run` WebSocket message makes the document take a snapshot of its text, call Piston in a spawned task, and broadcast `RunStarted` / `RunFinished` / `RunFailed` over the existing broadcast channel. The React client shows a Run button in the sidebar and an output panel under the editor.

**Tech Stack:** Rust (warp 0.3, tokio 1.38, reqwest 0.11 with rustls, semver 1), React 18 + Chakra UI 2 + Monaco, Piston v2 API, docker compose.

**Spec:** `docs/superpowers/specs/2026-10-09-code-execution-design.md`

## Global Constraints

- `PISTON_URL` unset means the feature is off and the server's messages are exactly as before; every existing test in `rustpad-server/tests/` passes unchanged except for the one `ServerConfig { .. }` literal in `persist.rs`.
- Rustpad calls only `GET /api/v2/runtimes` and `POST /api/v2/execute`. Never `/api/v2/packages`.
- Piston client timeouts: connect 2 s, total 20 s (constructor fields).
- Response body cap: 1 MiB (`1024 * 1024` bytes); larger is an `InvalidResponse` error.
- Per-stream output cap: 64 KiB (`64 * 1024` bytes), cut on a UTF-8 boundary, `truncated: true`.
- Per-request limits: `run_timeout` 3000, `compile_timeout` 10000, `run_memory_limit` and `compile_memory_limit` 268435456.
- User-facing errors, verbatim: `Code runner is unavailable`, `Code runner timed out`, `Code runner returned an invalid response`, `No runtime for <language>`.
- Runtime refresh interval: 5 minutes.
- Override table, verbatim: `cpp` → `c++`, `shell` → `bash`.
- Program output renders only as React text children: no `dangerouslySetInnerHTML`, no ANSI-to-HTML, no auto-linking.
- Commit messages: subject line only, no body, no ticket ID, no co-author trailer.

## Review Focus

1. A user changes the language while a run is in progress: the result must be labelled with the language and runtime the run used, not the new one. Test: `test_language_change_during_run` in Task 4.
2. A pad where nobody ever chose a language: Run must fail with `No runtime for plaintext`, not panic or hang. Test: `test_run_without_language` in Task 4.
3. The person who pressed Run leaves before it finishes: the others must still get the result. Test: `test_runner_leaves_mid_run` in Task 4.
4. Piston kills a program (timeout, out of memory): `code` is `null` and `signal` is `"SIGKILL"`, and both must deserialize and reach the client. Test: `test_execute_killed_by_signal` in Task 2.
5. Piston rejects a request with 400 (for example, our limits are above its configured ceiling): the user gets `Code runner is unavailable` and the server logs Piston's message. Test: `test_execute_bad_request` in Task 2.

---

## File map

| File | Status | Responsibility |
|---|---|---|
| `rustpad-server/Cargo.toml` | Modify | Add `reqwest`, `semver` |
| `rustpad-server/src/piston.rs` | Create | Piston types, client, runtime resolution, `Runner` and refresher |
| `rustpad-server/src/lib.rs` | Modify | `ServerConfig.piston`, `ServerState.runner`, wire runner into documents |
| `rustpad-server/src/rustpad.rs` | Modify | `Run` message, run state, `Runtimes` / run broadcasts, replay |
| `rustpad-server/src/main.rs` | Modify | Read `PISTON_URL` |
| `rustpad-server/tests/common/mod.rs` | Modify | Declare `pub mod piston;` |
| `rustpad-server/tests/common/piston.rs` | Create | Fake Piston server for tests |
| `rustpad-server/tests/piston_client.rs` | Create | HTTP client tests against the fake |
| `rustpad-server/tests/run.rs` | Create | End-to-end WebSocket run tests |
| `rustpad-server/tests/persist.rs` | Modify | Add `..ServerConfig::default()` to the literal |
| `src/rustpad.ts` | Modify | Types, callbacks, `run()` |
| `src/App.tsx` | Modify | Runtime and run state, Ctrl/Cmd+Enter, render `RunOutput` |
| `src/Sidebar.tsx` | Modify | Run button and hint |
| `src/RunOutput.tsx` | Create | Output panel |
| `docker-compose/docker-compose.yml` | Create | Local stack: Rustpad + Piston |
| `docker-compose/install-runtimes.sh` | Create | Install default Piston packages (dev only) |
| `README.md` | Modify | `PISTON_URL` and code execution section |
| `docs/superpowers/specs/2026-10-09-code-execution-design.md` | Modify | `run` may be `null` (Task 1) |

---

### Task 1: Piston types, runtime resolution and output truncation

Pure functions with no I/O. Piston's execute response omits `run` when the compile stage fails, so `run` is optional; this task also records that in the spec.

**Files:**
- Modify: `rustpad-server/Cargo.toml`
- Create: `rustpad-server/src/piston.rs`
- Modify: `rustpad-server/src/lib.rs` (module declaration only)
- Modify: `docs/superpowers/specs/2026-10-09-code-execution-design.md`

**Interfaces:**
- Produces (all in `rustpad_server::piston`):
  - `pub struct PistonRuntime { pub language: String, pub version: String, pub aliases: Vec<String> }` (Deserialize)
  - `pub struct Runtime { pub language: String, pub runtime: String, pub version: String }` (Serialize, Deserialize, Clone, Debug, PartialEq)
  - `pub struct RunStage { pub stdout: String, pub stderr: String, pub code: Option<i32>, pub signal: Option<String>, pub truncated: bool }` (Serialize, Deserialize, Clone, Debug, Default, PartialEq)
  - `pub struct ExecOutput { pub compile: Option<RunStage>, pub run: Option<RunStage> }` (Deserialize, Clone, Debug, PartialEq)
  - `pub fn resolve(monaco: &[String], available: &[PistonRuntime]) -> Vec<Runtime>`
  - `pub fn monaco_languages() -> Vec<String>`
  - `fn truncate_utf8(text: &mut String, max: usize) -> bool` (private)
  - `impl RunStage { fn truncate(&mut self) }` (private)
  - Constants: `MAX_STREAM_BYTES = 64 * 1024`

- [ ] **Step 1: Add dependencies**

In `rustpad-server/Cargo.toml`, add under `[dependencies]` in alphabetical order. Use reqwest 0.11 because it shares hyper 0.14 with warp 0.3.

```toml
reqwest = { version = "0.11", default-features = false, features = ["json", "rustls-tls"] }
semver = "1.0"
```

- [ ] **Step 2: Write the failing tests**

Create `rustpad-server/src/piston.rs` with only the test module, so it fails to compile:

```rust
//! Client for the Piston code execution engine.

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
```

In `rustpad-server/src/lib.rs`, after `pub mod database;` add:

```rust
pub mod piston;
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p rustpad-server --lib piston`
Expected: compile errors such as `cannot find function 'resolve'` and `cannot find type 'PistonRuntime'`.

- [ ] **Step 4: Write the implementation**

Insert above the `#[cfg(test)]` module in `rustpad-server/src/piston.rs`:

```rust
use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

/// Largest stdout or stderr kept from one stage, in bytes.
pub const MAX_STREAM_BYTES: usize = 64 * 1024;

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
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p rustpad-server --lib piston`
Expected: 12 tests PASS. Dead-code warnings for `truncate` are fine until Task 2.

- [ ] **Step 6: Update the spec**

In `docs/superpowers/specs/2026-10-09-code-execution-design.md`, under "Wire protocol", replace the line

```
- `compile` has the same shape as `run`, or is `null` for interpreted
  languages.
```

with

```
- `compile` has the same shape as `run`, or is `null` for interpreted
  languages. `run` is `null` when compilation fails.
```

- [ ] **Step 7: Commit**

```bash
git add rustpad-server/Cargo.toml Cargo.lock rustpad-server/src/piston.rs rustpad-server/src/lib.rs docs/superpowers/specs/2026-10-09-code-execution-design.md
git commit -m "Add Piston types and runtime resolution"
```

---

### Task 2: Piston HTTP client and fake Piston

**Files:**
- Modify: `rustpad-server/src/piston.rs`
- Modify: `rustpad-server/tests/common/mod.rs`
- Create: `rustpad-server/tests/common/piston.rs`
- Create: `rustpad-server/tests/piston_client.rs`

**Interfaces:**
- Consumes: `PistonRuntime`, `RunStage`, `ExecOutput`, `RunStage::truncate` from Task 1.
- Produces (in `rustpad_server::piston`):
  - `pub enum RunError { Unavailable, TimedOut, InvalidResponse }` with `Display` giving the verbatim messages from Global Constraints. Clone, Copy, Debug, PartialEq, Eq.
  - `#[derive(Clone, Debug)] pub struct PistonClient`
  - `pub fn PistonClient::new(base_url: &str) -> anyhow::Result<Self>` (2 s / 20 s)
  - `pub fn PistonClient::with_timeouts(base_url: &str, connect: Duration, total: Duration) -> anyhow::Result<Self>`
  - `pub async fn runtimes(&self) -> Result<Vec<PistonRuntime>, RunError>`
  - `pub async fn execute(&self, language: &str, version: &str, source: &str) -> Result<ExecOutput, RunError>`
- Produces (test helper `common::piston`):
  - `pub enum Reply { Json(Value), Status(u16), Slow(Duration, Value), Raw(String) }`
  - `pub struct FakePiston { pub url: String, pub requests: Arc<Mutex<Vec<Value>>> }`
  - `pub async fn start(runtimes: Value, reply: Reply) -> FakePiston`
  - `pub fn default_runtimes() -> Value`
  - `pub fn ok_reply(stdout: &str) -> Value`

- [ ] **Step 1: Write the fake Piston**

Create `rustpad-server/tests/common/piston.rs`:

```rust
//! A fake Piston server for tests.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{json, Value};
use warp::{http::StatusCode, Filter};

/// How the fake answers `POST /api/v2/execute`.
#[derive(Clone)]
pub enum Reply {
    /// 200 with this JSON body.
    Json(Value),
    /// This status with a Piston-style error body.
    Status(u16),
    /// 200 with this JSON body, after a delay.
    Slow(Duration, Value),
    /// 200 with this raw body.
    Raw(String),
}

/// A running fake Piston.
pub struct FakePiston {
    /// Base URL, such as `http://127.0.0.1:41234`.
    pub url: String,
    /// JSON bodies received on `/api/v2/execute`, in order.
    pub requests: Arc<Mutex<Vec<Value>>>,
}

/// Runtimes served by default: two Python versions and C++.
pub fn default_runtimes() -> Value {
    json!([
        { "language": "python", "version": "3.9.4", "aliases": ["py"] },
        { "language": "python", "version": "3.12.0", "aliases": ["py", "python3"] },
        { "language": "c++", "version": "10.2.0", "aliases": ["cpp", "g++"] }
    ])
}

/// A successful Python run that printed `stdout`.
pub fn ok_reply(stdout: &str) -> Value {
    json!({
        "language": "python",
        "version": "3.12.0",
        "run": { "stdout": stdout, "stderr": "", "output": stdout, "code": 0, "signal": null }
    })
}

/// Starts a fake Piston on a random local port.
pub async fn start(runtimes: Value, reply: Reply) -> FakePiston {
    let requests = Arc::new(Mutex::new(Vec::new()));

    let runtimes_route = warp::path!("api" / "v2" / "runtimes")
        .and(warp::get())
        .map(move || warp::reply::json(&runtimes));

    let recorded = Arc::clone(&requests);
    let execute_route = warp::path!("api" / "v2" / "execute")
        .and(warp::post())
        .and(warp::body::json())
        .then(move |body: Value| {
            recorded.lock().push(body);
            let reply = reply.clone();
            async move {
                match reply {
                    Reply::Json(value) => warp::reply::with_status(value.to_string(), StatusCode::OK),
                    Reply::Status(code) => warp::reply::with_status(
                        json!({ "message": "fake failure" }).to_string(),
                        StatusCode::from_u16(code).unwrap(),
                    ),
                    Reply::Slow(delay, value) => {
                        tokio::time::sleep(delay).await;
                        warp::reply::with_status(value.to_string(), StatusCode::OK)
                    }
                    Reply::Raw(body) => warp::reply::with_status(body, StatusCode::OK),
                }
            }
        });

    let (addr, server) =
        warp::serve(runtimes_route.or(execute_route)).bind_ephemeral(([127, 0, 0, 1], 0));
    tokio::spawn(server);
    FakePiston {
        url: format!("http://{addr}"),
        requests,
    }
}
```

At the top of `rustpad-server/tests/common/mod.rs`, add:

```rust
pub mod piston;
```

- [ ] **Step 2: Write the failing client tests**

Create `rustpad-server/tests/piston_client.rs`:

```rust
//! Tests for the Piston HTTP client against a fake Piston.

use std::time::Duration;

use common::piston::*;
use rustpad_server::piston::{PistonClient, RunError};
use serde_json::json;

pub mod common;

fn client(url: &str) -> PistonClient {
    PistonClient::with_timeouts(url, Duration::from_secs(1), Duration::from_millis(300)).unwrap()
}

#[tokio::test]
async fn test_runtimes() {
    let fake = start(default_runtimes(), Reply::Json(ok_reply(""))).await;
    let runtimes = client(&fake.url).runtimes().await.unwrap();
    assert_eq!(runtimes.len(), 3);
    assert_eq!(runtimes[2].language, "c++");
    assert_eq!(runtimes[2].aliases, vec!["cpp", "g++"]);
}

#[tokio::test]
async fn test_execute_sends_source_and_limits() {
    let fake = start(default_runtimes(), Reply::Json(ok_reply("hi\n"))).await;
    let output = client(&fake.url)
        .execute("python", "3.12.0", "print('hi')")
        .await
        .unwrap();

    let run = output.run.unwrap();
    assert_eq!(run.stdout, "hi\n");
    assert_eq!(run.code, Some(0));
    assert!(output.compile.is_none());

    let requests = fake.requests.lock();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0],
        json!({
            "language": "python",
            "version": "3.12.0",
            "files": [{ "content": "print('hi')" }],
            "compile_timeout": 10000,
            "run_timeout": 3000,
            "compile_memory_limit": 268435456,
            "run_memory_limit": 268435456
        })
    );
}

#[tokio::test]
async fn test_execute_compile_error() {
    let reply = json!({
        "language": "c++",
        "version": "10.2.0",
        "compile": { "stdout": "", "stderr": "error: expected ';'", "output": "error: expected ';'", "code": 1, "signal": null }
    });
    let fake = start(default_runtimes(), Reply::Json(reply)).await;
    let output = client(&fake.url).execute("c++", "10.2.0", "int main(){}").await.unwrap();
    assert!(output.run.is_none());
    let compile = output.compile.unwrap();
    assert_eq!(compile.code, Some(1));
    assert_eq!(compile.stderr, "error: expected ';'");
}

#[tokio::test]
async fn test_execute_killed_by_signal() {
    let reply = json!({
        "language": "python",
        "version": "3.12.0",
        "run": { "stdout": "", "stderr": "", "output": "", "code": null, "signal": "SIGKILL" }
    });
    let fake = start(default_runtimes(), Reply::Json(reply)).await;
    let run = client(&fake.url)
        .execute("python", "3.12.0", "while True: pass")
        .await
        .unwrap()
        .run
        .unwrap();
    assert_eq!(run.code, None);
    assert_eq!(run.signal.as_deref(), Some("SIGKILL"));
}

#[tokio::test]
async fn test_execute_truncates_output() {
    let fake = start(default_runtimes(), Reply::Json(ok_reply(&"x".repeat(100 * 1024)))).await;
    let run = client(&fake.url)
        .execute("python", "3.12.0", "")
        .await
        .unwrap()
        .run
        .unwrap();
    assert_eq!(run.stdout.len(), 64 * 1024);
    assert!(run.truncated);
}

#[tokio::test]
async fn test_execute_server_error() {
    let fake = start(default_runtimes(), Reply::Status(500)).await;
    let err = client(&fake.url).execute("python", "3.12.0", "").await.unwrap_err();
    assert_eq!(err, RunError::Unavailable);
    assert_eq!(err.to_string(), "Code runner is unavailable");
}

#[tokio::test]
async fn test_execute_bad_request() {
    let fake = start(default_runtimes(), Reply::Status(400)).await;
    let err = client(&fake.url).execute("python", "3.12.0", "").await.unwrap_err();
    assert_eq!(err, RunError::Unavailable);
}

#[tokio::test]
async fn test_execute_timeout() {
    let fake = start(
        default_runtimes(),
        Reply::Slow(Duration::from_millis(800), ok_reply("late")),
    )
    .await;
    let err = client(&fake.url).execute("python", "3.12.0", "").await.unwrap_err();
    assert_eq!(err, RunError::TimedOut);
    assert_eq!(err.to_string(), "Code runner timed out");
}

#[tokio::test]
async fn test_execute_oversized_body() {
    let fake = start(default_runtimes(), Reply::Raw("x".repeat(2 * 1024 * 1024))).await;
    let err = client(&fake.url).execute("python", "3.12.0", "").await.unwrap_err();
    assert_eq!(err, RunError::InvalidResponse);
    assert_eq!(err.to_string(), "Code runner returned an invalid response");
}

#[tokio::test]
async fn test_execute_invalid_json() {
    let fake = start(default_runtimes(), Reply::Raw("not json".into())).await;
    let err = client(&fake.url).execute("python", "3.12.0", "").await.unwrap_err();
    assert_eq!(err, RunError::InvalidResponse);
}

#[tokio::test]
async fn test_connection_refused() {
    // Port 1 on localhost has no listener.
    let err = client("http://127.0.0.1:1").runtimes().await.unwrap_err();
    assert_eq!(err, RunError::Unavailable);
}

#[test]
fn test_rejects_invalid_url() {
    assert!(PistonClient::new("not a url").is_err());
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p rustpad-server --test piston_client`
Expected: compile errors `cannot find type 'PistonClient'` and `cannot find type 'RunError'`.

- [ ] **Step 4: Write the client**

In `rustpad-server/src/piston.rs`, change the imports at the top to:

```rust
use std::cmp::Ordering;
use std::fmt;
use std::time::Duration;

use anyhow::Context;
use log::warn;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
```

Add these constants after `MAX_STREAM_BYTES`:

```rust
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
```

Add after `ExecOutput`:

```rust
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
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p rustpad-server --test piston_client && cargo test -p rustpad-server --lib piston`
Expected: 12 client tests and 12 unit tests PASS.

- [ ] **Step 6: Run the full suite**

Run: `cargo test -p rustpad-server`
Expected: all PASS. The new `pub mod piston;` in `common` must not break the other test files.

- [ ] **Step 7: Commit**

```bash
git add rustpad-server/src/piston.rs rustpad-server/tests/common rustpad-server/tests/piston_client.rs
git commit -m "Add Piston HTTP client"
```

---

### Task 3: Runtime discovery and `Runtimes` message

**Files:**
- Modify: `rustpad-server/src/piston.rs`
- Modify: `rustpad-server/src/lib.rs`
- Modify: `rustpad-server/src/rustpad.rs`
- Modify: `rustpad-server/src/main.rs`
- Modify: `rustpad-server/tests/persist.rs:66-69`
- Create: `rustpad-server/tests/run.rs`

**Interfaces:**
- Consumes: `PistonClient`, `PistonClient::runtimes`, `resolve`, `monaco_languages`, `Runtime` (Tasks 1–2); `common::piston::{start, Reply, default_runtimes, ok_reply, FakePiston}`.
- Produces:
  - `ServerConfig.piston: Option<PistonClient>` (public field, `None` in `Default`).
  - `pub(crate) struct Runner { pub(crate) client: PistonClient, pub(crate) runtimes: watch::Receiver<Vec<Runtime>> }` with `pub(crate) fn Runner::start(client: PistonClient) -> Runner`.
  - `Rustpad::with_runner(self, runner: Option<Runner>) -> Self` (`pub(crate)`).
  - Field `Rustpad.runner: Option<Runner>`.
  - `ServerMsg::Runtimes(Vec<Runtime>)`, serialized `{"Runtimes":[...]}`.
  - Test helpers in `tests/run.rs`: `config(&FakePiston) -> ServerConfig`, `expect_runtimes(&mut JsonSocket) -> Result<Value>`.

- [ ] **Step 1: Fix the `ServerConfig` literal in `persist.rs`**

In `rustpad-server/tests/persist.rs`, change

```rust
    let filter = server(ServerConfig {
        expiry_days: 2,
        database: Some(Database::new(&temp_sqlite_uri()?).await?),
    });
```

to

```rust
    let filter = server(ServerConfig {
        expiry_days: 2,
        database: Some(Database::new(&temp_sqlite_uri()?).await?),
        ..ServerConfig::default()
    });
```

- [ ] **Step 2: Write the failing tests**

Create `rustpad-server/tests/run.rs`:

```rust
//! Tests for running code through Piston.

use std::time::Duration;

use anyhow::{bail, Result};
use common::piston::*;
use common::*;
use rustpad_server::{piston::PistonClient, server, ServerConfig};
use serde_json::{json, Value};

pub mod common;

/// A server config pointing at the fake, with short timeouts.
fn config(fake: &FakePiston) -> ServerConfig {
    ServerConfig {
        piston: Some(
            PistonClient::with_timeouts(&fake.url, Duration::from_secs(1), Duration::from_millis(300))
                .unwrap(),
        ),
        ..ServerConfig::default()
    }
}

/// Reads messages until a non-empty `Runtimes` arrives, and returns its list.
/// The first `Runtimes` may be empty if the server hasn't fetched yet.
async fn expect_runtimes(client: &mut JsonSocket) -> Result<Value> {
    loop {
        let msg = client.recv().await?;
        match msg.get("Runtimes") {
            Some(list) if list.as_array().is_some_and(|a| !a.is_empty()) => return Ok(list.clone()),
            Some(_) => continue,
            None => bail!("expected Runtimes, got {msg}"),
        }
    }
}

#[tokio::test]
async fn test_runtimes_on_connect() -> Result<()> {
    pretty_env_logger::try_init().ok();
    let fake = start(default_runtimes(), Reply::Json(ok_reply(""))).await;
    let filter = server(config(&fake));

    let mut client = connect(&filter, "pad").await?;
    assert_eq!(client.recv().await?, json!({ "Identity": 0 }));
    let runtimes = expect_runtimes(&mut client).await?;
    let runtimes = runtimes.as_array().unwrap();

    assert!(runtimes.contains(&json!({ "language": "python", "runtime": "python", "version": "3.12.0" })));
    assert!(runtimes.contains(&json!({ "language": "cpp", "runtime": "c++", "version": "10.2.0" })));
    assert!(!runtimes.iter().any(|r| r["language"] == "markdown"));
    Ok(())
}

#[tokio::test]
async fn test_no_runtimes_when_off() -> Result<()> {
    pretty_env_logger::try_init().ok();
    let filter = server(ServerConfig::default());

    let mut client = connect(&filter, "pad").await?;
    assert_eq!(client.recv().await?, json!({ "Identity": 0 }));

    // The next message must be the reply to ClientInfo, not Runtimes.
    let alice = json!({ "name": "Alice", "hue": 42 });
    client.send(&json!({ "ClientInfo": alice })).await;
    assert_eq!(
        client.recv().await?,
        json!({ "UserInfo": { "id": 0, "info": alice } })
    );
    Ok(())
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p rustpad-server --test run`
Expected: compile error `struct 'ServerConfig' has no field named 'piston'`.

- [ ] **Step 4: Add `Runner` and the refresher to `piston.rs`**

Change the imports at the top of `rustpad-server/src/piston.rs` to:

```rust
use std::cmp::Ordering;
use std::fmt;
use std::time::Duration;

use anyhow::Context;
use log::{info, warn};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::{sync::watch, time};
```

Add a constant after `REQUEST_TIMEOUT`:

```rust
/// How often the runtime list is fetched again.
const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
```

Add before `#[cfg(test)]`:

```rust
/// Piston access shared by every document.
#[derive(Clone, Debug)]
pub(crate) struct Runner {
    /// Client used to run code.
    pub(crate) client: PistonClient,
    /// Current list of runnable languages, refreshed in the background.
    pub(crate) runtimes: watch::Receiver<Vec<Runtime>>,
}

impl Runner {
    /// Starts fetching runtimes in the background.
    pub(crate) fn start(client: PistonClient) -> Self {
        let (tx, rx) = watch::channel(Vec::new());
        tokio::spawn(refresher(client.clone(), tx));
        Self {
            client,
            runtimes: rx,
        }
    }
}

/// Fetches the runtime list now and then every `REFRESH_INTERVAL`.
async fn refresher(client: PistonClient, tx: watch::Sender<Vec<Runtime>>) {
    let monaco = monaco_languages();
    while !tx.is_closed() {
        match client.runtimes().await {
            Ok(available) => {
                let resolved = resolve(&monaco, &available);
                tx.send_if_modified(|current| {
                    if *current == resolved {
                        return false;
                    }
                    info!("Piston runtimes: {} languages", resolved.len());
                    *current = resolved;
                    true
                });
            }
            Err(e) => warn!("failed to fetch Piston runtimes: {e}"),
        }
        time::sleep(REFRESH_INTERVAL).await;
    }
}
```

- [ ] **Step 5: Wire it into `lib.rs`**

In `rustpad-server/src/lib.rs`:

Change `use crate::{database::Database, rustpad::Rustpad};` to

```rust
use crate::{
    database::Database,
    piston::{PistonClient, Runner},
    rustpad::Rustpad,
};
```

Add to `ServerState` after `database`:

```rust
    /// Piston access for running code, if enabled.
    runner: Option<Runner>,
```

Add to `ServerConfig` after `database`:

```rust
    /// Piston client for running code, if enabled.
    pub piston: Option<PistonClient>,
```

In `impl Default for ServerConfig`, add `piston: None,` after `database: None,`.

In `backend()`, replace

```rust
    let state = ServerState {
        documents: Default::default(),
        database: config.database,
    };
```

with

```rust
    let state = ServerState {
        documents: Default::default(),
        database: config.database,
        runner: config.piston.map(Runner::start),
    };
```

In `socket_handler()`, replace

```rust
            let rustpad = Arc::new(match &state.database {
                Some(db) => db.load(&id).await.map(Rustpad::from).unwrap_or_default(),
                None => Rustpad::default(),
            });
```

with

```rust
            let rustpad = match &state.database {
                Some(db) => db.load(&id).await.map(Rustpad::from).unwrap_or_default(),
                None => Rustpad::default(),
            };
            let rustpad = Arc::new(rustpad.with_runner(state.runner.clone()));
```

- [ ] **Step 6: Send `Runtimes` from `rustpad.rs`**

In `rustpad-server/src/rustpad.rs`:

Change the `tokio::sync` import and add the piston import:

```rust
use tokio::sync::{broadcast, watch, Notify};
use warp::ws::{Message, WebSocket};

use crate::piston::{Runner, Runtime};
use crate::{database::PersistedDocument, ot::transform_index};
```

Add a field to `struct Rustpad` after `killed`:

```rust
    /// Piston access, if code execution is enabled.
    runner: Option<Runner>,
```

and `runner: None,` in `impl Default for Rustpad`.

Add a variant to `ServerMsg` after `UserCursor`:

```rust
    /// Lists the languages this server can run.
    Runtimes(Vec<Runtime>),
```

Add this method to `impl Rustpad`, after `killed()`:

```rust
    /// Enables code execution for this document.
    pub(crate) fn with_runner(mut self, runner: Option<Runner>) -> Self {
        self.runner = runner;
        self
    }
```

In `handle_connection()`, replace

```rust
        let mut update_rx = self.update.subscribe();

        let mut revision: usize = self.send_initial(id, &mut socket).await?;
```

with

```rust
        let mut update_rx = self.update.subscribe();
        let mut runtimes_rx = self.runner.as_ref().map(|r| r.runtimes.clone());

        let mut revision: usize = self
            .send_initial(id, &mut socket, runtimes_rx.as_mut())
            .await?;
```

and add a branch to the `tokio::select!`, after the `update = update_rx.recv()` branch:

```rust
                Some(runtimes) = next_runtimes(&mut runtimes_rx) => {
                    socket.send(ServerMsg::Runtimes(runtimes).into()).await?;
                }
```

Change the `send_initial` signature and add the `Runtimes` message after the cursor loop, before `state.operations.len()`:

```rust
    async fn send_initial(
        &self,
        id: u64,
        socket: &mut WebSocket,
        runtimes_rx: Option<&mut watch::Receiver<Vec<Runtime>>>,
    ) -> Result<usize> {
```

```rust
            if let Some(rx) = runtimes_rx {
                messages.push(ServerMsg::Runtimes(rx.borrow_and_update().clone()));
            }
            state.operations.len()
```

Add a free function at the end of the file:

```rust
/// Waits for the runtime list to change, or forever if code execution is off.
async fn next_runtimes(rx: &mut Option<watch::Receiver<Vec<Runtime>>>) -> Option<Vec<Runtime>> {
    match rx {
        Some(rx) => {
            rx.changed().await.ok()?;
            Some(rx.borrow_and_update().clone())
        }
        None => future::pending().await,
    }
}
```

`future` comes from the existing `use futures::prelude::*;`.

- [ ] **Step 7: Read `PISTON_URL` in `main.rs`**

In `rustpad-server/src/main.rs`, change the import to

```rust
use rustpad_server::{database::Database, piston::PistonClient, server, ServerConfig};
```

and add to the `ServerConfig` literal after `database`:

```rust
        piston: std::env::var("PISTON_URL")
            .ok()
            .map(|url| PistonClient::new(&url).expect("Unable to parse PISTON_URL")),
```

- [ ] **Step 8: Run tests to verify they pass**

Run: `cargo test -p rustpad-server`
Expected: all PASS, including `test_runtimes_on_connect`, `test_no_runtimes_when_off`, and every existing test.

- [ ] **Step 9: Commit**

```bash
git add rustpad-server
git commit -m "Send runnable languages to clients"
```

---

### Task 4: `Run` message, shared results and replay

**Files:**
- Modify: `rustpad-server/src/rustpad.rs`
- Modify: `rustpad-server/tests/run.rs`

**Interfaces:**
- Consumes: `Runner { client, runtimes }`, `PistonClient::execute`, `RunStage`, `RunError` (`Display`), `ServerMsg::Runtimes` (Tasks 1–3); `config`, `expect_runtimes` in `tests/run.rs`.
- Produces:
  - `ClientMsg::Run`, received as the JSON string `"Run"`.
  - `ServerMsg::RunStarted { id: u64 }`
  - `ServerMsg::RunFinished { id: u64, language: String, runtime: String, version: String, compile: Option<RunStage>, run: Option<RunStage>, duration_ms: u64 }`
  - `ServerMsg::RunFailed { id: u64, error: String }`

- [ ] **Step 1: Write the failing tests**

Add these imports to `rustpad-server/tests/run.rs`:

```rust
use operational_transform::OperationSeq;
use warp::{filters::BoxedFilter, Reply as WarpReply};
```

The alias avoids a clash with `common::piston::Reply`.

Append to `rustpad-server/tests/run.rs`:

```rust
/// Connects the first user and sets the pad's language and text.
async fn open_pad(
    filter: &BoxedFilter<(impl WarpReply + 'static,)>,
    id: &str,
    language: &str,
    text: &str,
) -> Result<JsonSocket> {
    let mut client = connect(filter, id).await?;
    assert_eq!(client.recv().await?, json!({ "Identity": 0 }));
    expect_runtimes(&mut client).await?;
    client.send(&json!({ "SetLanguage": language })).await;
    assert_eq!(client.recv().await?, json!({ "Language": language }));
    let mut operation = OperationSeq::default();
    operation.insert(text);
    client
        .send(&json!({ "Edit": { "revision": 0, "operation": operation } }))
        .await;
    assert!(client.recv().await?.get("History").is_some());
    Ok(client)
}

/// Connects another user to a pad that already has text and a language.
async fn join_pad(
    filter: &BoxedFilter<(impl WarpReply + 'static,)>,
    id: &str,
    user: u64,
) -> Result<JsonSocket> {
    let mut client = connect(filter, id).await?;
    assert_eq!(client.recv().await?, json!({ "Identity": user }));
    assert!(client.recv().await?.get("History").is_some());
    assert!(client.recv().await?.get("Language").is_some());
    expect_runtimes(&mut client).await?;
    Ok(client)
}

#[tokio::test]
async fn test_shared_run() -> Result<()> {
    pretty_env_logger::try_init().ok();
    let fake = start(default_runtimes(), Reply::Json(ok_reply("hi\n"))).await;
    let filter = server(config(&fake));
    let mut alice = open_pad(&filter, "pad", "python", "print('hi')").await?;
    let mut bob = join_pad(&filter, "pad", 1).await?;

    alice.send(&json!("Run")).await;

    for client in [&mut alice, &mut bob] {
        assert_eq!(client.recv().await?, json!({ "RunStarted": { "id": 0 } }));
        let finished = client.recv().await?;
        let finished = &finished["RunFinished"];
        assert_eq!(finished["id"], 0);
        assert_eq!(finished["language"], "python");
        assert_eq!(finished["runtime"], "python");
        assert_eq!(finished["version"], "3.12.0");
        assert_eq!(finished["compile"], Value::Null);
        assert_eq!(
            finished["run"],
            json!({ "stdout": "hi\n", "stderr": "", "code": 0, "signal": null, "truncated": false })
        );
        assert!(finished["duration_ms"].is_u64());
    }

    let requests = fake.requests.lock();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["files"][0]["content"], "print('hi')");
    assert_eq!(requests[0]["language"], "python");
    assert_eq!(requests[0]["version"], "3.12.0");
    Ok(())
}

#[tokio::test]
async fn test_compile_error() -> Result<()> {
    pretty_env_logger::try_init().ok();
    let reply = json!({
        "language": "c++",
        "version": "10.2.0",
        "compile": { "stdout": "", "stderr": "error", "output": "error", "code": 1, "signal": null }
    });
    let fake = start(default_runtimes(), Reply::Json(reply)).await;
    let filter = server(config(&fake));
    let mut alice = open_pad(&filter, "pad", "cpp", "int main() {").await?;

    alice.send(&json!("Run")).await;
    assert_eq!(alice.recv().await?, json!({ "RunStarted": { "id": 0 } }));
    let finished = alice.recv().await?;
    assert_eq!(finished["RunFinished"]["runtime"], "c++");
    assert_eq!(finished["RunFinished"]["compile"]["stderr"], "error");
    assert_eq!(finished["RunFinished"]["run"], Value::Null);
    assert_eq!(fake.requests.lock()[0]["language"], "c++");
    Ok(())
}

#[tokio::test]
async fn test_busy_run_ignored() -> Result<()> {
    pretty_env_logger::try_init().ok();
    let fake = start(
        default_runtimes(),
        Reply::Slow(Duration::from_millis(150), ok_reply("once")),
    )
    .await;
    let filter = server(config(&fake));
    let mut alice = open_pad(&filter, "pad", "python", "print('once')").await?;

    alice.send(&json!("Run")).await;
    alice.send(&json!("Run")).await;
    assert_eq!(alice.recv().await?, json!({ "RunStarted": { "id": 0 } }));
    assert!(alice.recv().await?.get("RunFinished").is_some());

    // The second Run arrived during the first and must not reach Piston.
    assert_eq!(fake.requests.lock().len(), 1);
    Ok(())
}

#[tokio::test]
async fn test_language_change_during_run() -> Result<()> {
    pretty_env_logger::try_init().ok();
    let fake = start(
        default_runtimes(),
        Reply::Slow(Duration::from_millis(150), ok_reply("hi\n")),
    )
    .await;
    let filter = server(config(&fake));
    let mut alice = open_pad(&filter, "pad", "python", "print('hi')").await?;

    alice.send(&json!("Run")).await;
    alice.send(&json!({ "SetLanguage": "cpp" })).await;
    assert_eq!(alice.recv().await?, json!({ "RunStarted": { "id": 0 } }));
    assert_eq!(alice.recv().await?, json!({ "Language": "cpp" }));
    let finished = alice.recv().await?;
    assert_eq!(finished["RunFinished"]["language"], "python");
    assert_eq!(finished["RunFinished"]["runtime"], "python");
    Ok(())
}

#[tokio::test]
async fn test_unsupported_language() -> Result<()> {
    pretty_env_logger::try_init().ok();
    let fake = start(default_runtimes(), Reply::Json(ok_reply(""))).await;
    let filter = server(config(&fake));
    let mut alice = open_pad(&filter, "pad", "markdown", "# hi").await?;

    alice.send(&json!("Run")).await;
    assert_eq!(
        alice.recv().await?,
        json!({ "RunFailed": { "id": 0, "error": "No runtime for markdown" } })
    );
    assert!(fake.requests.lock().is_empty());
    Ok(())
}

#[tokio::test]
async fn test_run_without_language() -> Result<()> {
    pretty_env_logger::try_init().ok();
    let fake = start(default_runtimes(), Reply::Json(ok_reply(""))).await;
    let filter = server(config(&fake));
    let mut alice = connect(&filter, "pad").await?;
    assert_eq!(alice.recv().await?, json!({ "Identity": 0 }));
    expect_runtimes(&mut alice).await?;

    alice.send(&json!("Run")).await;
    assert_eq!(
        alice.recv().await?,
        json!({ "RunFailed": { "id": 0, "error": "No runtime for plaintext" } })
    );
    Ok(())
}

#[tokio::test]
async fn test_piston_error() -> Result<()> {
    pretty_env_logger::try_init().ok();
    let fake = start(default_runtimes(), Reply::Status(500)).await;
    let filter = server(config(&fake));
    let mut alice = open_pad(&filter, "pad", "python", "1").await?;

    alice.send(&json!("Run")).await;
    assert_eq!(alice.recv().await?, json!({ "RunStarted": { "id": 0 } }));
    assert_eq!(
        alice.recv().await?,
        json!({ "RunFailed": { "id": 0, "error": "Code runner is unavailable" } })
    );
    Ok(())
}

#[tokio::test]
async fn test_piston_timeout() -> Result<()> {
    pretty_env_logger::try_init().ok();
    let fake = start(
        default_runtimes(),
        Reply::Slow(Duration::from_millis(800), ok_reply("late")),
    )
    .await;
    let filter = server(config(&fake));
    let mut alice = open_pad(&filter, "pad", "python", "1").await?;

    alice.send(&json!("Run")).await;
    assert_eq!(alice.recv().await?, json!({ "RunStarted": { "id": 0 } }));
    assert_eq!(
        alice.recv().await?,
        json!({ "RunFailed": { "id": 0, "error": "Code runner timed out" } })
    );
    Ok(())
}

#[tokio::test]
async fn test_late_joiner_sees_last_result() -> Result<()> {
    pretty_env_logger::try_init().ok();
    let fake = start(default_runtimes(), Reply::Json(ok_reply("hi\n"))).await;
    let filter = server(config(&fake));
    let mut alice = open_pad(&filter, "pad", "python", "print('hi')").await?;

    alice.send(&json!("Run")).await;
    alice.recv().await?; // RunStarted
    let finished = alice.recv().await?;

    let mut carol = join_pad(&filter, "pad", 1).await?;
    assert_eq!(carol.recv().await?, finished);
    Ok(())
}

#[tokio::test]
async fn test_late_joiner_sees_run_in_progress() -> Result<()> {
    pretty_env_logger::try_init().ok();
    let fake = start(
        default_runtimes(),
        Reply::Slow(Duration::from_millis(150), ok_reply("hi\n")),
    )
    .await;
    let filter = server(config(&fake));
    let mut alice = open_pad(&filter, "pad", "python", "print('hi')").await?;

    alice.send(&json!("Run")).await;
    assert_eq!(alice.recv().await?, json!({ "RunStarted": { "id": 0 } }));

    let mut carol = join_pad(&filter, "pad", 1).await?;
    assert_eq!(carol.recv().await?, json!({ "RunStarted": { "id": 0 } }));
    assert!(carol.recv().await?.get("RunFinished").is_some());
    Ok(())
}

#[tokio::test]
async fn test_runner_leaves_mid_run() -> Result<()> {
    pretty_env_logger::try_init().ok();
    let fake = start(
        default_runtimes(),
        Reply::Slow(Duration::from_millis(150), ok_reply("hi\n")),
    )
    .await;
    let filter = server(config(&fake));
    let mut alice = open_pad(&filter, "pad", "python", "print('hi')").await?;
    let mut bob = join_pad(&filter, "pad", 1).await?;

    alice.send(&json!("Run")).await;
    assert_eq!(bob.recv().await?, json!({ "RunStarted": { "id": 0 } }));
    drop(alice);

    // Bob gets Alice's departure and the result, in either order.
    let first = bob.recv().await?;
    let second = bob.recv().await?;
    assert!(
        first.get("RunFinished").is_some() || second.get("RunFinished").is_some(),
        "no RunFinished in {first} / {second}"
    );
    Ok(())
}

#[tokio::test]
async fn test_run_ignored_when_off() -> Result<()> {
    pretty_env_logger::try_init().ok();
    let filter = server(ServerConfig::default());
    let mut client = connect(&filter, "pad").await?;
    assert_eq!(client.recv().await?, json!({ "Identity": 0 }));

    client.send(&json!("Run")).await;
    let alice = json!({ "name": "Alice", "hue": 42 });
    client.send(&json!({ "ClientInfo": alice })).await;
    assert_eq!(
        client.recv().await?,
        json!({ "UserInfo": { "id": 0, "info": alice } })
    );
    Ok(())
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p rustpad-server --test run`
Expected: the new tests FAIL. The server can't parse `"Run"` and closes the socket, so `recv` errors.

- [ ] **Step 3: Implement**

In `rustpad-server/src/rustpad.rs`:

Change the std, parking_lot and piston imports:

```rust
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
```

```rust
use parking_lot::{Mutex, RwLock, RwLockUpgradableReadGuard};
```

```rust
use crate::piston::{RunStage, Runner, Runtime};
```

Add a field to `struct Rustpad` after `runner`:

```rust
    /// State of code runs, shared with the task running the code.
    run: Arc<Mutex<RunState>>,
```

and `run: Default::default(),` in `impl Default for Rustpad`.

Add after `struct State`:

```rust
/// Code run state for one document, kept in memory only.
#[derive(Default)]
struct RunState {
    /// User ID of the run in progress.
    running: Option<u64>,
    /// The last `RunFinished` or `RunFailed` message.
    last: Option<ServerMsg>,
}
```

Add a variant to `ClientMsg` after `CursorData`:

```rust
    /// Runs the current document with its language's runtime.
    Run,
```

Add variants to `ServerMsg` after `Runtimes`:

```rust
    /// Broadcasts that a user started a run.
    RunStarted { id: u64 },
    /// Broadcasts the result of a finished run.
    RunFinished {
        id: u64,
        language: String,
        runtime: String,
        version: String,
        compile: Option<RunStage>,
        run: Option<RunStage>,
        duration_ms: u64,
    },
    /// Broadcasts that a run could not complete.
    RunFailed { id: u64, error: String },
```

In `send_initial`, after the `Runtimes` push, add:

```rust
            let run = self.run.lock();
            if let Some(id) = run.running {
                messages.push(ServerMsg::RunStarted { id });
            } else if let Some(last) = &run.last {
                messages.push(last.clone());
            }
            drop(run);
```

In `handle_message`, add an arm after `ClientMsg::CursorData(data) => { ... }`:

```rust
            ClientMsg::Run => self.start_run(id),
```

Add this method to `impl Rustpad`, after `handle_message`:

```rust
    /// Starts running the document for user `id`, unless a run is in progress.
    fn start_run(&self, id: u64) {
        let Some(runner) = &self.runner else {
            warn!("run requested by id = {}, but code execution is off", id);
            return;
        };
        let (text, language) = {
            let state = self.state.read();
            let language = state.language.clone();
            (state.text.clone(), language.unwrap_or_else(|| "plaintext".into()))
        };
        let runtime = runner
            .runtimes
            .borrow()
            .iter()
            .find(|r| r.language == language)
            .cloned();

        let mut run = self.run.lock();
        if run.running.is_some() {
            info!("run requested by id = {} while busy, ignoring", id);
            return;
        }
        let Some(runtime) = runtime else {
            let msg = ServerMsg::RunFailed {
                id,
                error: format!("No runtime for {language}"),
            };
            run.last = Some(msg.clone());
            self.update.send(msg).ok();
            return;
        };
        run.running = Some(id);
        self.update.send(ServerMsg::RunStarted { id }).ok();
        drop(run);

        info!("run: id = {}, runtime = {} {}", id, runtime.runtime, runtime.version);
        let client = runner.client.clone();
        let run_state = Arc::clone(&self.run);
        let update = self.update.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let result = client
                .execute(&runtime.runtime, &runtime.version, &text)
                .await;
            let msg = match result {
                Ok(output) => ServerMsg::RunFinished {
                    id,
                    language: runtime.language,
                    runtime: runtime.runtime,
                    version: runtime.version,
                    compile: output.compile,
                    run: output.run,
                    duration_ms: started.elapsed().as_millis() as u64,
                },
                Err(e) => ServerMsg::RunFailed {
                    id,
                    error: e.to_string(),
                },
            };
            let mut run = run_state.lock();
            run.running = None;
            run.last = Some(msg.clone());
            update.send(msg).ok();
        });
    }
```

The broadcasts happen while `run` is locked, so `send_initial` never sees a state that disagrees with the message order. A client that subscribes in between may get the same result twice, from the replay and the broadcast. The UI treats that as the same result.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p rustpad-server`
Expected: all PASS, including the 12 new tests in `run.rs`.

- [ ] **Step 5: Run clippy and format**

Run: `cargo fmt --all && cargo clippy -p rustpad-server --all-targets -- -D warnings`
Expected: no warnings. Fix any it reports.

- [ ] **Step 6: Commit**

```bash
git add rustpad-server
git commit -m "Run code through Piston and share results"
```

---

### Task 5: Frontend client, state, Run button and shortcut

**Files:**
- Modify: `src/rustpad.ts`
- Modify: `src/App.tsx`
- Modify: `src/Sidebar.tsx`

**Interfaces:**
- Consumes: server messages from Tasks 3–4.
- Produces:
  - `export type Runtime = { language; runtime; version }`, `export type RunStage`, `export type RunResult` in `src/rustpad.ts`.
  - `RustpadOptions.onRuntimes?: (runtimes: Runtime[]) => void`
  - `RustpadOptions.onRunStarted?: (by: UserInfo | null) => void`
  - `RustpadOptions.onRunFinished?: (by: UserInfo | null, result: RunResult) => void`
  - `RustpadOptions.onRunFailed?: (by: UserInfo | null, error: string) => void`
  - `Rustpad.run(): boolean`
  - `export type RunState` in `src/RunOutput.tsx` (created here as a type-only file and filled in by Task 6).
  - New `Sidebar` props: `runtimes: Runtime[] | null`, `canRun: boolean`, `running: boolean`, `onRun: () => void`.

- [ ] **Step 1: Add types, callbacks and `run()` to `src/rustpad.ts`**

Add to `RustpadOptions` after `onChangeUsers`:

```ts
  readonly onRuntimes?: (runtimes: Runtime[]) => void;
  readonly onRunStarted?: (by: UserInfo | null) => void;
  readonly onRunFinished?: (by: UserInfo | null, result: RunResult) => void;
  readonly onRunFailed?: (by: UserInfo | null, error: string) => void;
```

Add after the `UserInfo` type:

```ts
/** A language this server can run, with the Piston runtime that runs it. */
export type Runtime = {
  readonly language: string;
  readonly runtime: string;
  readonly version: string;
};

/** Output of one stage (compile or run) of a code run. */
export type RunStage = {
  readonly stdout: string;
  readonly stderr: string;
  readonly code: number | null;
  readonly signal: string | null;
  readonly truncated: boolean;
};

/** Result of a finished code run. `run` is null when compilation failed. */
export type RunResult = {
  readonly language: string;
  readonly runtime: string;
  readonly version: string;
  readonly compile: RunStage | null;
  readonly run: RunStage | null;
  readonly duration_ms: number;
};
```

Add after `setInfo()`:

```ts
  /** Ask the server to run the document, if connected. */
  run(): boolean {
    this.ws?.send(`"Run"`);
    return this.ws !== undefined;
  }
```

In `handleMessage`, add after the `UserCursor` branch:

```ts
    } else if (msg.Runtimes !== undefined) {
      this.options.onRuntimes?.(msg.Runtimes);
    } else if (msg.RunStarted !== undefined) {
      this.options.onRunStarted?.(this.userInfo(msg.RunStarted.id));
    } else if (msg.RunFinished !== undefined) {
      const { id, ...result } = msg.RunFinished;
      this.options.onRunFinished?.(this.userInfo(id), result);
    } else if (msg.RunFailed !== undefined) {
      const { id, error } = msg.RunFailed;
      this.options.onRunFailed?.(this.userInfo(id), error);
    }
```

Add after `handleMessage`:

```ts
  /** Returns the info of a user in this pad, including this client. */
  private userInfo(id: number): UserInfo | null {
    if (id === this.me) return this.myInfo ?? null;
    return this.users[id] ?? null;
  }
```

Add to the `ServerMsg` type after `UserCursor`:

```ts
  Runtimes?: Runtime[];
  RunStarted?: { id: number };
  RunFinished?: RunResult & { id: number };
  RunFailed?: { id: number; error: string };
```

- [ ] **Step 2: Create the `RunState` type in `src/RunOutput.tsx`**

```tsx
import type { RunResult, UserInfo } from "./rustpad";

/** What the output panel shows. */
export type RunState = {
  status: "idle" | "running" | "finished" | "failed";
  by: UserInfo | null;
  result?: RunResult;
  error?: string;
};
```

- [ ] **Step 3: Add state and callbacks in `src/App.tsx`**

Change

```tsx
import Rustpad, { UserInfo } from "./rustpad";
```

to

```tsx
import type { RunState } from "./RunOutput";
import Rustpad, { Runtime, UserInfo } from "./rustpad";
```

Add after the `readCodeConfirmOpen` state:

```tsx
  const [runtimes, setRuntimes] = useState<Runtime[] | null>(null);
  const [run, setRun] = useState<RunState>({ status: "idle", by: null });
  const [outputOpen, setOutputOpen] = useState(false);
  const runRef = useRef(() => {});
  const canRunKey = useRef<editor.IContextKey<boolean>>();
```

In the `new Rustpad({ ... })` options, replace

```tsx
        onConnected: () => setConnection("connected"),
        onDisconnected: () => setConnection("disconnected"),
```

with

```tsx
        onConnected: () => {
          setConnection("connected");
          setRun({ status: "idle", by: null });
        },
        onDisconnected: () => {
          setConnection("disconnected");
          setRuntimes(null);
        },
```

and add after `onChangeUsers: setUsers,`:

```tsx
        onRuntimes: setRuntimes,
        onRunStarted: (by) => {
          setRun({ status: "running", by });
          setOutputOpen(true);
        },
        onRunFinished: (by, result) => {
          setRun({ status: "finished", by, result });
          setOutputOpen(true);
        },
        onRunFailed: (by, error) => {
          setRun({ status: "failed", by, error });
          setOutputOpen(true);
        },
```

Add after `handleDarkModeChange`:

```tsx
  const canRun =
    connection === "connected" &&
    run.status !== "running" &&
    (runtimes?.some((r) => r.language === language) ?? false);

  function handleRun() {
    if (canRun) rustpad.current?.run();
  }
  runRef.current = handleRun;

  useEffect(() => {
    canRunKey.current?.set(canRun);
  }, [canRun]);
```

Pass the new props to `<Sidebar>`, after `onChangeColor`:

```tsx
          runtimes={runtimes}
          canRun={canRun}
          running={run.status === "running"}
          onRun={handleRun}
```

Replace `onMount={(editor) => setEditor(editor)}` with:

```tsx
              onMount={(editor, monaco) => {
                setEditor(editor);
                // Ctrl/Cmd+Enter runs only when a run is possible; otherwise
                // Monaco's default "insert line below" applies.
                canRunKey.current = editor.createContextKey("rustpadCanRun", false);
                editor.addCommand(
                  monaco.KeyMod.CtrlCmd | monaco.KeyCode.Enter,
                  () => runRef.current(),
                  "rustpadCanRun",
                );
              }}
```

- [ ] **Step 4: Add the Run button to `src/Sidebar.tsx`**

Change the icon import to `import { VscPlay, VscRepo } from "react-icons/vsc";`, and change `import type { UserInfo } from "./rustpad";` to `import type { Runtime, UserInfo } from "./rustpad";`.

Add to `SidebarProps` after `onChangeColor`:

```ts
  runtimes: Runtime[] | null;
  canRun: boolean;
  running: boolean;
  onRun: () => void;
```

Add `runtimes, canRun, running, onRun,` to the destructured parameters after `onChangeColor,`.

Insert after the Language `</Select>`:

```tsx
      {runtimes !== null && (
        <>
          <Button
            size="sm"
            w="full"
            mt={2}
            colorScheme="green"
            leftIcon={<VscPlay />}
            isLoading={running}
            loadingText="Running"
            isDisabled={!canRun}
            onClick={onRun}
          >
            Run
          </Button>
          {!runtimes.some((r) => r.language === language) && (
            <Text fontSize="xs" mt={1} color={darkMode ? "gray.400" : "gray.600"}>
              No runtime for {language}
            </Text>
          )}
        </>
      )}
```

- [ ] **Step 5: Type-check and format**

Run: `npm run check && npx prettier --check src`
Expected: no errors. If prettier complains, run `npx prettier --write src` and check again.

- [ ] **Step 6: Commit**

```bash
git add src/rustpad.ts src/App.tsx src/Sidebar.tsx src/RunOutput.tsx
git commit -m "Add Run button and run state to the frontend"
```

---

### Task 6: Output panel

**Files:**
- Modify: `src/RunOutput.tsx`
- Modify: `src/App.tsx`

**Interfaces:**
- Consumes: `RunState` (Task 5), `RunResult`, `RunStage` from `src/rustpad.ts`; `run`, `outputOpen`, `setOutputOpen`, `darkMode` in `App`.
- Produces: `export default function RunOutput(props: { run: RunState; darkMode: boolean; onClose: () => void })`.

- [ ] **Step 1: Write the component**

Replace the contents of `src/RunOutput.tsx` with:

```tsx
import { Box, CloseButton, Flex, Spinner, Text } from "@chakra-ui/react";

import type { RunResult, RunStage, UserInfo } from "./rustpad";

/** What the output panel shows. */
export type RunState = {
  status: "idle" | "running" | "finished" | "failed";
  by: UserInfo | null;
  result?: RunResult;
  error?: string;
};

/** Monaco's default font stack, so output matches the editor. */
const monospace = 'Menlo, Monaco, "Courier New", monospace';

function hasOutput(stage: RunStage): boolean {
  return stage.code !== 0 || stage.stdout !== "" || stage.stderr !== "";
}

function summary(result: RunResult): string {
  const stage = result.run ?? result.compile;
  const outcome = stage?.signal ?? `exit ${stage?.code ?? "?"}`;
  const compileFailed = result.run === null ? "compile failed · " : "";
  return `${result.runtime} ${result.version} · ${result.duration_ms} ms · ${compileFailed}${outcome}`;
}

function Stream(props: { text: string; error?: boolean; darkMode: boolean }) {
  if (props.text === "") return null;
  const errorColor = props.darkMode ? "#f48771" : "#a1260d";
  return (
    <Box
      as="pre"
      m={0}
      fontFamily="inherit"
      whiteSpace="pre-wrap"
      wordBreak="break-all"
      color={props.error ? errorColor : undefined}
    >
      {props.text}
    </Box>
  );
}

function StageOutput(props: { stage: RunStage; darkMode: boolean }) {
  return (
    <>
      <Stream text={props.stage.stdout} darkMode={props.darkMode} />
      <Stream text={props.stage.stderr} error darkMode={props.darkMode} />
      {props.stage.truncated && <Text color="gray.500">[output truncated]</Text>}
    </>
  );
}

function RunOutput(props: { run: RunState; darkMode: boolean; onClose: () => void }) {
  const { run, darkMode } = props;
  const name = run.by?.name ?? "someone";
  const compile = run.result?.compile;

  let header = "";
  if (run.status === "running") header = `Running · started by ${name}`;
  if (run.status === "finished" && run.result) header = `Run by ${name} · ${summary(run.result)}`;
  if (run.status === "failed") header = `Run by ${name} · failed`;

  return (
    <Flex
      direction="column"
      h="30%"
      flexShrink={0}
      borderTopWidth="1px"
      borderColor={darkMode ? "#3c3c3c" : "#e8e8e8"}
      bgColor={darkMode ? "#252526" : "#f3f3f3"}
    >
      <Flex
        align="center"
        h={7}
        px={3.5}
        flexShrink={0}
        color="#888888"
        fontSize="13px"
        fontWeight="medium"
      >
        {run.status === "running" && <Spinner size="xs" mr={2} />}
        <Text flex={1} noOfLines={1}>
          {header}
        </Text>
        <CloseButton size="sm" onClick={props.onClose} />
      </Flex>
      <Box flex={1} minH={0} overflowY="auto" px={3.5} pb={2} fontFamily={monospace} fontSize="13px">
        {run.status === "failed" && <Stream text={run.error ?? ""} error darkMode={darkMode} />}
        {compile && hasOutput(compile) && <StageOutput stage={compile} darkMode={darkMode} />}
        {run.result?.run && <StageOutput stage={run.result.run} darkMode={darkMode} />}
      </Box>
    </Flex>
  );
}

export default RunOutput;
```

- [ ] **Step 2: Render it in `src/App.tsx`**

Change `import type { RunState } from "./RunOutput";` to

```tsx
import RunOutput, { type RunState } from "./RunOutput";
```

After the `<Box flex={1} minH={0}> ... </Box>` that wraps `<Editor>`, inside the same column `Flex`, add:

```tsx
          {outputOpen && run.status !== "idle" && (
            <RunOutput run={run} darkMode={darkMode} onClose={() => setOutputOpen(false)} />
          )}
```

- [ ] **Step 3: Check for unsafe rendering**

Run: `grep -rn "dangerouslySetInnerHTML\|innerHTML" src`
Expected: no matches.

- [ ] **Step 4: Type-check and format**

Run: `npm run check && npx prettier --check src`
Expected: no errors. Run `npx prettier --write src` if needed.

- [ ] **Step 5: Commit**

```bash
git add src/RunOutput.tsx src/App.tsx
git commit -m "Add run output panel"
```

---

### Task 7: Local compose stack, runtime install script and README

**Files:**
- Create: `docker-compose/docker-compose.yml`
- Create: `docker-compose/install-runtimes.sh`
- Modify: `README.md` (Configuration list and a new section before `## Deployment`)

**Interfaces:**
- Consumes: `PISTON_URL` (Task 3); request limits from Global Constraints.
- Produces: a stack started with `docker compose -f docker-compose/docker-compose.yml up --build`, with Piston on network `rustpad-piston` at `http://piston:2000`.

- [ ] **Step 1: Pin the Piston image and confirm its config keys**

Run:

```bash
docker buildx imagetools inspect ghcr.io/engineer-man/piston:latest --format '{{json .Manifest.Digest}}'
```

Record the `sha256:...` digest; Step 2 uses it as `<DIGEST>`. Then list the config keys:

```bash
docker run --rm --entrypoint cat ghcr.io/engineer-man/piston@<DIGEST> /piston_api/src/config.js | grep -n "key:"
```

Expected keys include `run_timeout`, `compile_timeout`, `run_memory_limit`, `compile_memory_limit`, `output_max_size`, `max_process_count`, `max_open_files`, `max_file_size`, `disable_networking`. Piston reads each as `PISTON_<KEY_UPPERCASE>`. If a key is missing or named differently, use the name in this file and note it in the commit.

- [ ] **Step 2: Write `docker-compose/docker-compose.yml`**

Replace `<DIGEST>` with the value from Step 1:

```yaml
# Local development stack: Rustpad with a Piston code runner.
# Run from the repository root:
#   docker compose -f docker-compose/docker-compose.yml up --build
#   docker-compose/install-runtimes.sh

services:
  rustpad:
    build: ..
    environment:
      PISTON_URL: http://piston:2000
      RUST_LOG: info
    ports:
      - 127.0.0.1:3030:3030
    depends_on:
      - piston

  piston:
    image: ghcr.io/engineer-man/piston@<DIGEST>
    # Piston's isolate sandbox needs cgroups, which needs a privileged container.
    privileged: true
    environment:
      # Ceilings for every run. Rustpad's per-request limits must not exceed them.
      PISTON_RUN_TIMEOUT: "3000"
      PISTON_COMPILE_TIMEOUT: "10000"
      PISTON_RUN_MEMORY_LIMIT: "268435456"
      PISTON_COMPILE_MEMORY_LIMIT: "268435456"
      # Slightly above Rustpad's 64 KiB cut, so truncation shows in the UI,
      # and well below its 1 MiB response cap.
      PISTON_OUTPUT_MAX_SIZE: "81920"
      PISTON_MAX_PROCESS_COUNT: "64"
      PISTON_MAX_OPEN_FILES: "256"
      PISTON_MAX_FILE_SIZE: "10000000"
      PISTON_DISABLE_NETWORKING: "true"
    volumes:
      - piston-packages:/piston/packages
    tmpfs:
      - /piston/jobs:exec,uid=1000,gid=1000,mode=711
      - /tmp:exec

volumes:
  piston-packages:

networks:
  default:
    name: rustpad-piston
```

The Piston service has no `ports:` entry. Only containers on the compose network can reach it.

- [ ] **Step 3: Write `docker-compose/install-runtimes.sh`**

```bash
#!/usr/bin/env bash
# Installs the default Piston runtimes into the local compose stack.
# Needs internet access. Run from the repository root while the stack is up.
set -euo pipefail

packages=(python node typescript go rust java gcc mono bash)

piston() {
  docker run --rm --network rustpad-piston curlimages/curl:8.10.1 -fsS "$@"
}

for package in "${packages[@]}"; do
  echo "Installing ${package}"
  piston -X POST http://piston:2000/api/v2/packages \
    -H 'Content-Type: application/json' \
    -d "{\"language\":\"${package}\",\"version\":\"*\"}"
  echo
done

echo "Installed runtimes:"
piston http://piston:2000/api/v2/runtimes
echo
```

Run: `chmod +x docker-compose/install-runtimes.sh`

- [ ] **Step 4: Bring the stack up and install runtimes**

Run:

```bash
docker compose -f docker-compose/docker-compose.yml up --build -d
docker-compose/install-runtimes.sh
```

Expected: each package prints a JSON success body, and the final list includes python, javascript, typescript, go, rust, java, c, c++, csharp and bash. If Piston rejects `"version":"*"`, run `piston http://piston:2000/api/v2/packages` to list versions, and put exact versions in the script as `name=version` pairs.

Then run `docker compose -f docker-compose/docker-compose.yml logs rustpad | grep "Piston runtimes"`. Within 5 minutes of startup, or right after a restart of `rustpad`, it shows a non-zero count. Restart with `docker compose -f docker-compose/docker-compose.yml restart rustpad` to pick up the new runtimes immediately.

- [ ] **Step 5: Update `README.md`**

In the `## Configuration` list, add after the `SQLITE_URI` bullet:

```markdown
- `PISTON_URL`: Base URL of a [Piston](https://github.com/engineer-man/piston)
  server, such as `http://piston:2000`. If set, users can run the pad's code and
  everyone in the pad sees the output. Piston must be reachable only from
  Rustpad.
```

Insert before `## Deployment`:

````markdown
## Code execution

To run Rustpad with a local Piston, start the compose stack from the repository
root and install the default runtimes:

```
docker compose -f docker-compose/docker-compose.yml up --build -d
docker-compose/install-runtimes.sh
docker compose -f docker-compose/docker-compose.yml restart rustpad
```

Then open `http://localhost:3030`. The Run button appears under the language
picker, and `Ctrl/Cmd+Enter` runs the code from the editor.

Piston runs untrusted code in a privileged container. A production deployment
must meet these requirements:

- Piston runs on a dedicated node pool with no cloud permissions.
- Only Rustpad pods can reach Piston, and Piston has no egress.
- Runtimes are baked into the Piston image.
- Piston's `PISTON_*` limits are at least Rustpad's per-request limits: 3 s run,
  10 s compile, 256 MiB memory. Keep `PISTON_OUTPUT_MAX_SIZE` between 64 KiB
  and 128 KiB.

See `docs/superpowers/specs/2026-10-09-code-execution-design.md` for the full
list.
````

- [ ] **Step 6: Commit**

```bash
git add docker-compose README.md
git commit -m "Add local Piston stack and docs"
```

---

### Task 8: Manual and security check

Run against the compose stack from Task 7. Open two browser windows on the same pad (`http://localhost:3030/#check`).

**Files:** none changed unless a check fails. Fix any failure in the owning task's files, rerun `cargo test -p rustpad-server` and `npm run check`, and commit the fix with a subject naming it.

- [ ] **Step 1: Rebuild with the latest code**

Run: `docker compose -f docker-compose/docker-compose.yml up --build -d`

- [ ] **Step 2: Work through the checklist**

For each item, record pass or fail.

- [ ] Hello world in Python, JavaScript, TypeScript, Go, Rust, Java, C, C++, C# and shell.
- [ ] C++ with a missing `;`: the panel shows compile output, and the header reads `compile failed · exit 1`.
- [ ] Python `while True: pass`: killed at about 3 s, and the header shows `SIGKILL`.
- [ ] Python fork bomb, `import os` then `while True: os.fork()`: the run ends, and the host and the Rustpad container stay responsive.
- [ ] Python `x = bytearray(2 * 1024**3)`: fails with a memory error or `SIGKILL`.
- [ ] Python `print("x" * 10_000_000)`: the output ends with `[output truncated]`.
- [ ] Python `print('<img src=x onerror=alert(1)>'); print('\x1b[31mred\x1b[0m')`: both show as literal text, with no alert and no colour.
- [ ] Python `import socket; socket.create_connection(("1.1.1.1", 80), timeout=2)`: fails.
- [ ] Shell `curl -sS https://example.com`: fails.
- [ ] Window A runs; window B shows the same header and output, naming A's user.
- [ ] A third window opened after a run shows the last output.
- [ ] `docker compose -f docker-compose/docker-compose.yml stop piston`, then Run: the panel shows `Code runner is unavailable`. Start Piston again afterwards.
- [ ] Switch the language to markdown: Run is disabled, and the hint `No runtime for markdown` shows.
- [ ] `Ctrl/Cmd+Enter` runs Python; in markdown it inserts a line below.
- [ ] Start `docker run --rm -p 3030:3030` from the Rustpad image without `PISTON_URL`: no Run button appears.

- [ ] **Step 3: Report**

List each item with pass or fail. List any fixes and the commits that made them.
