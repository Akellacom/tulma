use std::sync::LazyLock;

use regex_lite::{Captures, Regex};
use serde_json::Value;

pub const MASK: &str = "***";

pub fn is_sensitive_key(key: &str) -> bool {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)token|secret|key|password|auth").unwrap());
    RE.is_match(key)
}

pub fn redact_text(text: &str) -> String {
    static BEARER: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\b(bearer|basic)(\s+)[A-Za-z0-9._~+/=-]+").unwrap());
    static ASSIGN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?i)\b([a-z0-9_-]*(?:token|secret|key|password|auth)[a-z0-9_-]*)(\s*[=:]\s*)([^\s&"',;]+)"#)
            .unwrap()
    });
    let text = BEARER.replace_all(text, |c: &Captures| format!("{}{}{MASK}", &c[1], &c[2]));
    ASSIGN
        .replace_all(&text, |c: &Captures| {
            let value = &c[3];
            let reference = value.starts_with('$');
            if reference || value == MASK || value.eq_ignore_ascii_case("bearer") || value.eq_ignore_ascii_case("basic") {
                c[0].to_string()
            } else {
                format!("{}{}{MASK}", &c[1], &c[2])
            }
        })
        .into_owned()
}

pub fn redact_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let v = if is_sensitive_key(k) {
                        Value::String(MASK.into())
                    } else {
                        redact_json(v)
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_json).collect()),
        Value::String(s) => Value::String(redact_text(s)),
        other => other.clone(),
    }
}

/// Truncates on a char boundary so previews never split a UTF-8 sequence.
pub fn truncate(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sensitive_keys_match_case_insensitively() {
        for key in ["GITHUB_PERSONAL_ACCESS_TOKEN", "EXA_API_KEY", "Authorization", "db_password", "HELPSCOUT_APP_SECRET"] {
            assert!(is_sensitive_key(key), "{key}");
        }
        for key in ["MEMORY_FILE_PATH", "HELPSCOUT_DEFAULT_INBOX_ID", "PATH"] {
            assert!(!is_sensitive_key(key), "{key}");
        }
    }

    #[test]
    fn bearer_tokens_and_query_secrets_are_masked() {
        assert_eq!(
            redact_text("Authorization: Bearer sk-abc.def_123 rest"),
            "Authorization: Bearer *** rest"
        );
        assert_eq!(
            redact_text("GET https://x.io/api/mcp?token=abc123&page=2"),
            "GET https://x.io/api/mcp?token=***&page=2"
        );
        assert_eq!(redact_text("api_key=xyz password: hunter2"), "api_key=*** password: ***");
        assert_eq!(redact_text("plain line"), "plain line");
        assert_eq!(redact_text("Authorization:${AUTH_HEADER} token=$TOKEN"), "Authorization:${AUTH_HEADER} token=$TOKEN");
    }

    #[test]
    fn json_redaction_masks_sensitive_fields_recursively() {
        let input = json!({
            "query": "rust",
            "apiKey": "abc",
            "nested": {"auth": {"user": "u"}, "items": ["Bearer zzz"]},
            "count": 3
        });
        assert_eq!(
            redact_json(&input),
            json!({
                "query": "rust",
                "apiKey": "***",
                "nested": {"auth": "***", "items": ["Bearer ***"]},
                "count": 3
            })
        );
    }
}
