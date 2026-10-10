use serde::Serialize;
use serde_json::Value;

/// Push message for the UI; serialized as a JSON-RPC notification on the control channel.
#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

impl Notification {
    pub fn new(method: &str, params: impl Serialize) -> Self {
        Self {
            method: method.to_string(),
            params: serde_json::to_value(params).unwrap_or(Value::Null),
        }
    }
}
