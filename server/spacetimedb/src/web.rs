//! HTTP plumbing shared by the routes: answers, request bodies, query parameters.

use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use spacetimedb::http::{Body, Request, Response};

/// HTTP status and JSON body of an answer; 204 goes out without a body.
pub type Answer = (u16, Value);

/// Largest request body read (store notifications carry certificate chains; client requests are small).
pub const MAX_BODY_BYTES: usize = 256 * 1024;

/// `{"status": ..., "message": ...}`.
pub fn answer(http: u16, status: &str, message: impl Into<String>) -> Answer {
    (http, json!({ "status": status, "message": message.into() }))
}

pub fn respond((http, body): Answer) -> Response {
    let builder = http::Response::builder().status(http);
    if http == 204 {
        return builder.body(Body::empty()).expect("valid response");
    }
    builder.header("Content-Type", "application/json").body(Body::from(body.to_string())).expect("valid response")
}

/// The request body, refused when it is larger than [`MAX_BODY_BYTES`].
pub fn body(req: Request) -> Result<Vec<u8>, Answer> {
    let bytes = req.into_body().into_bytes();
    if bytes.len() > MAX_BODY_BYTES {
        return Err(answer(413, "error", format!("request body over {MAX_BODY_BYTES} bytes")));
    }
    Ok(bytes.to_vec())
}

/// The request body as text (invalid UTF-8 replaced).
pub fn text(req: Request) -> Result<String, Answer> {
    body(req).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// The request body parsed as JSON.
pub fn json<T: DeserializeOwned>(req: Request) -> Result<T, Answer> {
    let bytes = body(req)?;
    serde_json::from_slice(&bytes).map_err(|e| answer(400, "error", format!("invalid JSON body: {e}")))
}

/// The percent-decoded query parameter `name`, empty when missing.
pub fn query_param(req: &Request, name: &str) -> String {
    purser_core::encoding::query_param(req.uri().query().unwrap_or(""), name).unwrap_or_default()
}

/// A secret from the request equals the expected one (compared in constant time).
pub fn secret_matches(expected: &str, got: &str) -> bool {
    purser_core::encoding::constant_time_eq(expected.as_bytes(), got.as_bytes())
}
