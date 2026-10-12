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
    let output = client(&fake.url)
        .execute("c++", "10.2.0", "int main(){}")
        .await
        .unwrap();
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
    let fake = start(
        default_runtimes(),
        Reply::Json(ok_reply(&"x".repeat(100 * 1024))),
    )
    .await;
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
    let err = client(&fake.url)
        .execute("python", "3.12.0", "")
        .await
        .unwrap_err();
    assert_eq!(err, RunError::Unavailable);
    assert_eq!(err.to_string(), "Code runner is unavailable");
}

#[tokio::test]
async fn test_execute_bad_request() {
    let fake = start(default_runtimes(), Reply::Status(400)).await;
    let err = client(&fake.url)
        .execute("python", "3.12.0", "")
        .await
        .unwrap_err();
    assert_eq!(err, RunError::Unavailable);
}

#[tokio::test]
async fn test_execute_timeout() {
    let fake = start(
        default_runtimes(),
        Reply::Slow(Duration::from_millis(800), ok_reply("late")),
    )
    .await;
    let err = client(&fake.url)
        .execute("python", "3.12.0", "")
        .await
        .unwrap_err();
    assert_eq!(err, RunError::TimedOut);
    assert_eq!(err.to_string(), "Code runner timed out");
}

#[tokio::test]
async fn test_execute_oversized_body() {
    let fake = start(default_runtimes(), Reply::Raw("x".repeat(5 * 1024 * 1024))).await;
    let err = client(&fake.url)
        .execute("python", "3.12.0", "")
        .await
        .unwrap_err();
    assert_eq!(err, RunError::InvalidResponse);
    assert_eq!(err.to_string(), "Code runner returned an invalid response");
}

#[tokio::test]
async fn test_execute_invalid_json() {
    let fake = start(default_runtimes(), Reply::Raw("not json".into())).await;
    let err = client(&fake.url)
        .execute("python", "3.12.0", "")
        .await
        .unwrap_err();
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
