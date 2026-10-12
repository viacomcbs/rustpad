//! Tests for running code through Piston.

use std::time::Duration;

use anyhow::{bail, Result};
use common::piston::*;
use common::*;
use operational_transform::OperationSeq;
use rustpad_server::{piston::PistonClient, server, ServerConfig};
use serde_json::{json, Value};
use warp::{filters::BoxedFilter, Reply as WarpReply};

pub mod common;

/// A server config pointing at the fake, with short timeouts.
fn config(fake: &FakePiston) -> ServerConfig {
    ServerConfig {
        piston: Some(
            PistonClient::with_timeouts(
                &fake.url,
                Duration::from_secs(1),
                Duration::from_millis(300),
            )
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

    assert!(runtimes
        .contains(&json!({ "language": "python", "runtime": "python", "version": "3.12.0" })));
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
async fn test_code_too_large() -> Result<()> {
    pretty_env_logger::try_init().ok();
    let fake = start(default_runtimes(), Reply::Json(ok_reply(""))).await;
    let filter = server(config(&fake));
    let mut alice = open_pad(&filter, "pad", "python", &"x".repeat(64 * 1024 + 1)).await?;

    alice.send(&json!("Run")).await;
    assert_eq!(
        alice.recv().await?,
        json!({ "RunFailed": { "id": 0, "error": "Code too large to run (64 KiB limit)" } })
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
