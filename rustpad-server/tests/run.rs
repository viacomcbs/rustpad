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
