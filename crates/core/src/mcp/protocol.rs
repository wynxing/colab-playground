//! The messages the local agent endpoint speaks.
//!
//! MCP rides on JSON-RPC 2.0: a request carries an `id` and gets exactly one reply, a
//! notification carries none and gets none. The revision this app implements is
//! [`PROTOCOL_VERSION`]; a client that asks for the revision before it is answered the same way,
//! and one that asks for a revision the app does not speak is told the one it does, which is what
//! the specification asks a server to do rather than failing the connection.
//!
//! Nothing here touches the network or the app's state: this file turns bytes into a request and a
//! result into bytes, so it can be tested on its own.

use serde_json::{Value, json};

/// The MCP revision this endpoint implements.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Revisions this endpoint can answer with: the one above, and the one before it.
pub const SUPPORTED_VERSIONS: [&str; 2] = ["2025-06-18", "2025-03-26"];

// The specification's own codes, sent to the client as they are.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// One message from a client, parsed far enough to route it.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    /// Wants one reply.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// Wants no reply.
    Notification { method: String, params: Value },
}

impl Incoming {
    pub fn method(&self) -> &str {
        match self {
            Incoming::Request { method, .. } | Incoming::Notification { method, .. } => method,
        }
    }
}

/// A JSON-RPC error reply, with the code the specification defines.
#[derive(Debug, PartialEq)]
pub struct Fault {
    pub code: i64,
    pub message: String,
    /// The request's id, when there was one to echo. A parse error has none.
    pub id: Option<Value>,
}

impl Fault {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Fault {
            code,
            message: message.into(),
            id: None,
        }
    }

    /// The same error, addressed to one request.
    pub fn to(mut self, id: &Value) -> Self {
        self.id = Some(id.clone());
        self
    }

    /// The body to send back.
    pub fn response(&self) -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": self.id.clone().unwrap_or(Value::Null),
            "error": { "code": self.code, "message": self.message },
        })
    }
}

/// Parses one message. The whole body must be one JSON-RPC 2.0 request or notification: anything
/// else is a fault the client sees, never a panic.
pub fn parse(body: &[u8]) -> std::result::Result<Incoming, Fault> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| Fault::new(PARSE_ERROR, format!("the body is not JSON: {e}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| Fault::new(INVALID_REQUEST, "not a JSON-RPC object"))?;
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(Fault::new(INVALID_REQUEST, "not JSON-RPC 2.0"));
    }
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| Fault::new(INVALID_REQUEST, "no method"))?
        .to_owned();
    let params = object.get("params").cloned().unwrap_or(Value::Null);
    Ok(match object.get("id") {
        // A null id is legal and must be answered; a missing one is a notification.
        Some(id) => Incoming::Request {
            id: id.clone(),
            method,
            params,
        },
        None => Incoming::Notification { method, params },
    })
}

/// A successful reply to one request.
pub fn result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// The revision to answer with: the client's when this app speaks it, otherwise this app's.
pub fn negotiate_version(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|asked| SUPPORTED_VERSIONS.iter().find(|known| **known == asked))
        .copied()
        .unwrap_or(PROTOCOL_VERSION)
}

/// The reply to `initialize`.
pub fn initialize_result(
    server_name: &str,
    server_version: &str,
    requested: Option<&str>,
) -> Value {
    json!({
        "protocolVersion": negotiate_version(requested),
        "capabilities": {
            "tools": { "listChanged": false },
            "logging": {},
        },
        "serverInfo": { "name": server_name, "version": server_version },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(text: &str) -> Vec<u8> {
        text.as_bytes().to_vec()
    }

    #[test]
    fn a_request_with_an_id_is_a_request() {
        let parsed = parse(&body(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{}}"#,
        ))
        .expect("valid");
        assert_eq!(
            parsed,
            Incoming::Request {
                id: json!(7),
                method: "tools/list".to_owned(),
                params: json!({}),
            }
        );
    }

    #[test]
    fn without_an_id_it_is_a_notification() {
        let parsed = parse(&body(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        ))
        .expect("valid");
        assert_eq!(
            parsed,
            Incoming::Notification {
                method: "notifications/initialized".to_owned(),
                params: Value::Null,
            }
        );
    }

    #[test]
    fn a_null_id_is_still_a_request() {
        let parsed = parse(&body(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#)).expect("valid");
        assert!(matches!(parsed, Incoming::Request { .. }));
    }

    #[test]
    fn the_body_must_be_jsonrpc_2() {
        for text in [
            "not json at all",
            "[]",
            r#"{"id":1,"method":"x"}"#,
            r#"{"jsonrpc":"1.0","id":1,"method":"x"}"#,
            r#"{"jsonrpc":"2.0","id":1}"#,
        ] {
            let fault = parse(&body(text)).expect_err(text);
            assert!(
                fault.code == PARSE_ERROR || fault.code == INVALID_REQUEST,
                "{text}: {}",
                fault.code
            );
            assert_eq!(fault.id, None, "a fault about the body names no request");
        }
    }

    #[test]
    fn a_fault_addressed_to_a_request_echoes_its_id() {
        let fault = Fault::new(METHOD_NOT_FOUND, "no such tool").to(&json!("abc"));
        assert_eq!(
            fault.response(),
            json!({
                "jsonrpc": "2.0",
                "id": "abc",
                "error": { "code": -32601, "message": "no such tool" }
            })
        );
    }

    #[test]
    fn the_revision_answered_is_the_clients_when_it_is_known() {
        assert_eq!(negotiate_version(Some("2025-03-26")), "2025-03-26");
        assert_eq!(negotiate_version(Some(PROTOCOL_VERSION)), PROTOCOL_VERSION);
        assert_eq!(negotiate_version(Some("2024-11-05")), PROTOCOL_VERSION);
        assert_eq!(negotiate_version(None), PROTOCOL_VERSION);
    }

    #[test]
    fn initialize_announces_tools_and_answers_the_revision() {
        let result = initialize_result("colab-playground", "0.5.1", Some("2025-03-26"));
        assert_eq!(result["protocolVersion"], "2025-03-26");
        assert_eq!(result["serverInfo"]["name"], "colab-playground");
        assert!(result["capabilities"]["tools"].is_object());
    }
}
