//! Conservative redaction for durable human/agent-authored text.

use std::sync::{Arc, OnceLock};

use regex::{Regex, RegexBuilder};
use serde_json::Value;

use crate::error::{Error, Result};

const SECRET_MARKERS: &[&str] = &[
    "api_key=",
    "apikey=",
    "access_token=",
    "auth_token=",
    "token=",
    "password=",
    "passwd=",
    "secret=",
    "aws_secret_access_key=",
    "aws_session_token=",
    "client_secret=",
    "authorization:",
    "bearer ",
];

#[derive(Clone, Debug, Default)]
pub struct Redactor {
    custom: Arc<Vec<Regex>>,
}

impl Redactor {
    /// Compile a bounded set of linear-time custom redaction expressions.
    ///
    /// # Errors
    ///
    /// Returns an error for too many, oversized, or invalid expressions.
    pub fn new(patterns: &[String]) -> Result<Self> {
        if patterns.len() > 32 {
            return Err(Error::Config(
                "security supports at most 32 custom redaction patterns".to_owned(),
            ));
        }
        let mut custom = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            if pattern.is_empty() || pattern.len() > 1_024 {
                return Err(Error::Config(
                    "custom redaction patterns must contain 1..=1024 bytes".to_owned(),
                ));
            }
            custom.push(
                RegexBuilder::new(pattern)
                    .size_limit(1_024 * 1_024)
                    .dfa_size_limit(1_024 * 1_024)
                    .build()
                    .map_err(|error| {
                        Error::Config(format!("invalid custom redaction pattern: {error}"))
                    })?,
            );
        }
        Ok(Self {
            custom: Arc::new(custom),
        })
    }

    #[must_use]
    pub fn redact_text(&self, value: &str) -> String {
        let mut output = redact_builtin(value);
        for pattern in self.custom.iter() {
            output = pattern.replace_all(&output, "[REDACTED]").into_owned();
        }
        output
    }

    #[must_use]
    pub fn redact_json(&self, value: &Value) -> Value {
        redact_json_with(value, self)
    }
}

/// Redact common credential forms before durable persistence.
#[must_use]
pub fn redact_text(value: &str) -> String {
    redact_builtin(value)
}

fn redact_builtin(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let mut output = redact_private_keys(value);
    output = assignment_regex()
        .replace_all(&output, "$1$2[REDACTED]")
        .into_owned();
    for marker in SECRET_MARKERS {
        output = redact_after_marker(&output, marker);
    }
    output
        .split_inclusive(char::is_whitespace)
        .map(redact_token_prefix)
        .collect()
}

/// Recursively redact strings and credential-shaped object fields.
#[must_use]
pub fn redact_json(value: &Value) -> Value {
    redact_json_with(value, &Redactor::default())
}

fn redact_json_with(value: &Value, redactor: &Redactor) -> Value {
    match value {
        Value::String(value) => Value::String(redactor.redact_text(value)),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| redact_json_with(value, redactor))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| {
                    let sensitive_key = {
                        let key = key.to_ascii_lowercase();
                        key.contains("password")
                            || key.contains("secret")
                            || key.contains("token")
                            || key.contains("api_key")
                            || key == "authorization"
                    };
                    (
                        key.clone(),
                        if sensitive_key {
                            Value::String("[REDACTED]".to_owned())
                        } else {
                            redact_json_with(value, redactor)
                        },
                    )
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn redact_after_marker(value: &str, marker: &str) -> String {
    let mut output = value.to_owned();
    let marker_lower = marker.to_ascii_lowercase();
    let mut search_from = 0;
    loop {
        let lower = output.to_ascii_lowercase();
        let Some(relative) = lower[search_from..].find(&marker_lower) else {
            break;
        };
        let start = search_from + relative + marker.len();
        let end = output[start..]
            .char_indices()
            .find(|(_, character)| {
                character.is_whitespace() || matches!(character, ',' | ';' | '"' | '\'')
            })
            .map_or(output.len(), |(offset, _)| start + offset);
        if end == start {
            search_from = start;
            continue;
        }
        output.replace_range(start..end, "[REDACTED]");
        search_from = start + "[REDACTED]".len();
    }
    output
}

fn redact_token_prefix(token: &str) -> &str {
    let trimmed = token.trim_end_matches(char::is_whitespace);
    let suffix = &token[trimmed.len()..];
    let looks_secret = (trimmed.starts_with("AKIA") && trimmed.len() >= 20)
        || trimmed.starts_with("ghp_")
        || trimmed.starts_with("github_pat_")
        || trimmed.starts_with("xoxb-")
        || (trimmed.starts_with("sk-") && trimmed.len() >= 16);
    if looks_secret {
        if suffix.is_empty() {
            "[REDACTED]"
        } else {
            // All callers use this only through `map(...).collect::<String>()`;
            // preserve a single separating space/newline in the common case.
            if suffix.contains('\n') {
                "[REDACTED]\n"
            } else {
                "[REDACTED] "
            }
        }
    } else {
        token
    }
}

fn redact_private_keys(value: &str) -> String {
    private_key_regex()
        .replace_all(value, "[REDACTED PRIVATE KEY]")
        .into_owned()
}

fn assignment_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r#"(?i)\b(api[_-]?key|apikey|access[_-]?token|auth[_-]?token|token|password|passwd|secret|aws[_-]?secret[_-]?access[_-]?key|aws[_-]?session[_-]?token|client[_-]?secret|private[_-]?token)\b(\s*[:=]\s*)(?:\"[^\"\r\n]*\"|'[^'\r\n]*'|[^\s,;]+)"#,
        )
        .expect("built-in assignment redaction regex is valid")
    })
}

fn private_key_regex() -> &'static Regex {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        Regex::new(
            r"(?s)-----BEGIN (?:RSA |EC |DSA |OPENSSH |ENCRYPTED )?PRIVATE KEY-----.*?-----END (?:RSA |EC |DSA |OPENSSH |ENCRYPTED )?PRIVATE KEY-----",
        )
        .expect("built-in private-key redaction regex is valid")
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn redacts_assignments_bearers_tokens_keys_and_nested_json() {
        let text = "api_key=abc AWS_SECRET_ACCESS_KEY = super-secret Bearer very-secret ghp_abcdefghijklmnop\n-----BEGIN OPENSSH PRIVATE KEY-----x-----END OPENSSH PRIVATE KEY-----";
        let redacted = redact_text(text);
        assert!(!redacted.contains("abc"));
        assert!(!redacted.contains("very-secret"));
        assert!(!redacted.contains("super-secret"));
        assert!(!redacted.contains("ghp_"));
        assert!(!redacted.contains("BEGIN PRIVATE"));
        let object = redact_json(&json!({
            "token": "visible-no-more",
            "nested": ["password=hunter2", 1]
        }));
        assert_eq!(object["token"], "[REDACTED]");
        assert!(!object.to_string().contains("hunter2"));
    }

    #[test]
    fn custom_redaction_regexes_are_bounded_and_applied_recursively() {
        let redactor = Redactor::new(&[r"ACME-[0-9]{8}".to_owned()]).expect("redactor");
        let value = redactor.redact_json(&json!({"note": "customer ACME-12345678"}));
        assert_eq!(value["note"], "customer [REDACTED]");
        assert!(Redactor::new(&["(".to_owned()]).is_err());
        assert!(Redactor::new(&vec!["x".to_owned(); 33]).is_err());
    }
}
