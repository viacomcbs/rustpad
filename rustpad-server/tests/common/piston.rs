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
