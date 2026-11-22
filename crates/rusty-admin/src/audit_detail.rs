//! What a change set, for the audit log (docs/design-audit.md): the fields of a JSON request body as a flat object of short text values. A value whose field name suggests a secret (a password, a key,
//! a token, a code, a certificate) is written as `(hidden)`, so the log shows that the field was sent and never what it held. Nothing but an object is summarised; the rest is long and not a setting.

use serde_json::{Map, Value};

const MAX_FIELDS: usize = 30;
const MAX_VALUE: usize = 120;
const MAX_TOTAL: usize = 3500;
const HIDDEN: &str = "(hidden)";
/// Names (lower case, as a part of the field name) whose values are never written.
const SECRET_PARTS: [&str; 10] = ["pass", "secret", "token", "private", "pem", "code", "totp", "cred", "seal", "key"];
/// Fields that carry the word but are settings, not secrets.
const SETTINGS: [&str; 2] = ["password_min_length", "password_classes"];

fn secret(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    !SETTINGS.iter().any(|s| n.ends_with(s)) && SECRET_PARTS.iter().any(|p| n.contains(p))
}

fn short(s: &str) -> String {
    if s.chars().count() <= MAX_VALUE {
        return s.to_owned();
    }
    let mut cut: String = s.chars().take(MAX_VALUE).collect();
    cut.push('…');
    cut
}

fn put(out: &mut Map<String, Value>, name: &str, v: &Value, depth: usize) {
    if out.len() >= MAX_FIELDS {
        return;
    }
    let text = match v {
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        // A boolean cannot be a secret; a number under a secret name can (a one-time code).
        Value::Number(n) => {
            if secret(name) {
                HIDDEN.to_owned()
            } else {
                n.to_string()
            }
        }
        Value::String(s) => {
            if secret(name) {
                HIDDEN.to_owned()
            } else {
                short(s)
            }
        }
        Value::Array(a) => {
            if secret(name) {
                HIDDEN.to_owned()
            } else if a.iter().all(|x| matches!(x, Value::String(_) | Value::Number(_) | Value::Bool(_))) {
                short(&a.iter().map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_owned)).collect::<Vec<_>>().join(", "))
            } else {
                format!("({} items)", a.len())
            }
        }
        Value::Object(o) => {
            if depth >= 1 || secret(name) {
                if secret(name) { HIDDEN.to_owned() } else { "(object)".to_owned() }
            } else {
                for (k, x) in o {
                    put(out, &format!("{name}.{k}"), x, depth + 1);
                }
                return;
            }
        }
    };
    out.insert(name.to_owned(), Value::String(text));
}

/// The summary of `body`, or `None` when it is not a JSON object with a field in it.
pub fn summarize(body: &[u8]) -> Option<String> {
    let Value::Object(o) = serde_json::from_slice::<Value>(body).ok()? else { return None };
    let mut out = Map::new();
    for (k, v) in &o {
        put(&mut out, k, v, 0);
    }
    if out.is_empty() {
        return None;
    }
    let mut s = serde_json::to_string(&out).ok()?;
    while s.len() > MAX_TOTAL {
        let last = out.keys().next_back()?.clone();
        out.remove(&last);
        s = serde_json::to_string(&out).ok()?;
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::summarize;

    #[test]
    fn values_are_shown_and_secrets_are_not() {
        let s = summarize(br#"{"quota_bytes":1048576,"virus_action":"reject","blocked_extensions":["exe","scr"],"password":"hunter2","secret_key":"abc","otp_code":123456,"key_set":true,"password_min_length":12,"x":null}"#).unwrap();
        for shown in ["\"quota_bytes\":\"1048576\"", "\"virus_action\":\"reject\"", "\"blocked_extensions\":\"exe, scr\"", "\"key_set\":\"true\"", "\"password_min_length\":\"12\"", "\"x\":\"null\""] {
            assert!(s.contains(shown), "{shown} in {s}");
        }
        for never in ["hunter2", "abc", "123456"] {
            assert!(!s.contains(never), "{never} leaked in {s}");
        }
        assert!(s.contains("\"password\":\"(hidden)\"") && s.contains("\"secret_key\":\"(hidden)\"") && s.contains("\"otp_code\":\"(hidden)\""));
    }

    #[test]
    fn nested_values_are_flattened_and_a_secret_inside_is_still_hidden() {
        let s = summarize(br#"{"s3":{"endpoint":"https://s3.example","secret_key":"zzz"},"deep":{"a":{"b":1}}}"#).unwrap();
        assert!(s.contains("\"s3.endpoint\":\"https://s3.example\"") && s.contains("\"s3.secret_key\":\"(hidden)\"") && s.contains("\"deep.a\":\"(object)\""), "{s}");
        assert!(!s.contains("zzz"));
    }

    #[test]
    fn long_values_and_many_fields_are_cut_and_other_bodies_are_not_summarised() {
        let long = "a".repeat(500);
        let s = summarize(format!("{{\"note\":\"{long}\"}}").as_bytes()).unwrap();
        assert!(s.len() < 200 && s.contains('…'));
        let many: Vec<String> = (0..100).map(|i| format!("\"f{i}\":\"{}\"", "v".repeat(100))).collect();
        assert!(summarize(format!("{{{}}}", many.join(",")).as_bytes()).unwrap().len() <= 3500);
        assert!(summarize(b"[1,2]").is_none() && summarize(b"not json").is_none() && summarize(b"{}").is_none() && summarize(b"").is_none());
    }
}
