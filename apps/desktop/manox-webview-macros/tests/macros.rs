//! Compile-and-exercise coverage for the proc-macro crate's four handlers.
//! Proc-macro crates cannot self-invoke in doctests (the crate is not a
//! dependency of itself), so the doc examples are `ignore`d and these tests
//! carry the real invocation coverage: each macro is expanded against a real
//! `http::Request<Vec<u8>>` and the generated handler's status / body is
//! asserted.

use serde::{Deserialize, Serialize};

use manox_webview_macros::{api_handler, api_handlers, command_handler, command_handlers};

#[derive(Deserialize, Serialize)]
struct Req {
    id: u32,
}

#[derive(Deserialize, Serialize)]
struct Resp {
    message: String,
}

fn echo(req: Req) -> Result<Resp, String> {
    Ok(Resp {
        message: format!("id={}", req.id),
    })
}

fn raw_handler(_req: http::Request<Vec<u8>>) -> http::Response<Vec<u8>> {
    http::Response::builder()
        .status(http::StatusCode::OK)
        .body(Vec::new())
        .unwrap()
}

#[test]
fn api_handler_returns_name_and_fn_pointer() {
    let (name, handler) = api_handler!(raw_handler);
    assert_eq!(name, "raw_handler");
    let resp = handler(http::Request::builder().body(Vec::new()).unwrap());
    assert_eq!(resp.status(), http::StatusCode::OK);
}

#[test]
fn api_handlers_batch_wraps_each_fn() {
    let handlers = api_handlers![raw_handler, raw_handler];
    assert_eq!(handlers.len(), 2);
    assert_eq!(handlers[0].0, "raw_handler");
    assert_eq!(handlers[1].0, "raw_handler");
}

#[test]
fn command_handler_ok_serializes_json() {
    let (name, handler) = command_handler!(echo);
    assert_eq!(name, "echo");
    let body = serde_json::to_vec(&Req { id: 7 }).unwrap();
    let req = http::Request::builder().body(body).unwrap();
    let resp = handler(req);
    assert_eq!(resp.status(), http::StatusCode::OK);
    let resp_body: Resp = serde_json::from_slice(resp.body()).unwrap();
    assert_eq!(resp_body.message, "id=7");
}

#[test]
fn command_handler_bad_request_on_bad_json() {
    let (_name, handler) = command_handler!(echo);
    let req = http::Request::builder().body(b"not json".to_vec()).unwrap();
    let resp = handler(req);
    assert_eq!(resp.status(), http::StatusCode::BAD_REQUEST);
}

#[test]
fn command_handler_internal_server_error_on_fn_err() {
    fn always_fail(_req: Req) -> Result<Resp, String> {
        Err("boom".into())
    }
    let (_name, handler) = command_handler!(always_fail);
    let body = serde_json::to_vec(&Req { id: 1 }).unwrap();
    let req = http::Request::builder().body(body).unwrap();
    let resp = handler(req);
    assert_eq!(resp.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
}

#[test]
fn command_handlers_batch_invokes_each() {
    let handlers = command_handlers![echo, echo];
    assert_eq!(handlers.len(), 2);
    assert_eq!(handlers[0].0, "echo");
    assert_eq!(handlers[1].0, "echo");
    let body = serde_json::to_vec(&Req { id: 9 }).unwrap();
    let req = http::Request::builder().body(body).unwrap();
    let resp = handlers[1].1(req);
    assert_eq!(resp.status(), http::StatusCode::OK);
}
