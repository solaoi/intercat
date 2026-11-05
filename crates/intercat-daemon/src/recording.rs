//! Persist completed text, never streaming fragments that can split secret values.
use serde_json::{Value, json};

const MAX_TEXT: usize = 1024 * 1024;

/// Status broadcasts and request presentations can point at another event's
/// cursor. Only durable view notifications own a cursor and advance replay.
pub fn view_cursor(frame: &Value) -> Option<&str> {
    if !frame["params"]["sourceRange"].is_object()
        || matches!(
            frame["method"].as_str(),
            Some("approval/request" | "userInput/request")
        )
    {
        return None;
    }
    frame["params"]["viewCursor"].as_str()
}

pub fn sanitize(mut frame: Value, secrets: &[String]) -> Value {
    if frame["method"]
        .as_str()
        .is_some_and(|name| name.ends_with("/delta"))
        && let Some(params) = frame.get_mut("params").and_then(Value::as_object_mut)
        && let Some(delta) = params.remove("delta")
    {
        params.insert(
            "deltaBytes".into(),
            json!(
                delta
                    .as_str()
                    .map_or_else(|| delta.to_string().len(), str::len)
            ),
        );
    }
    let mut truncated = Vec::new();
    scrub(&mut frame, secrets, "", &mut truncated);
    if !truncated.is_empty() {
        frame["_intercatTruncated"] = json!(truncated);
    }
    frame
}

fn scrub(value: &mut Value, secrets: &[String], path: &str, truncated: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            for secret in secrets.iter().filter(|s| !s.is_empty()) {
                *text = text.replace(secret, "[redacted]");
            }
            if text.len() > MAX_TEXT {
                let count = text.len();
                let head: String = text.chars().take(MAX_TEXT / 16).collect();
                let tail: String = text
                    .chars()
                    .rev()
                    .take(MAX_TEXT / 16)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                *text = format!("{head}\n[truncated: {count} bytes]\n{tail}");
                truncated.push(path.into());
            }
        }
        Value::Array(values) => {
            for (index, child) in values.iter_mut().enumerate() {
                scrub(child, secrets, &format!("{path}/{index}"), truncated);
            }
        }
        Value::Object(values) => {
            for (key, child) in values {
                scrub(child, secrets, &format!("{path}/{key}"), truncated);
            }
        }
        _ => {}
    }
}

/// The shape and technical IDs remain usable; display strings are redacted.
pub async fn redact(scanner: &crate::secret_scan::Scanner, value: Value) -> crate::Result<Value> {
    fn collect(value: &Value, paths: &mut Vec<String>, texts: &mut Vec<String>, path: String) {
        match value {
            Value::String(text) => {
                paths.push(path);
                texts.push(text.clone());
            }
            Value::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    collect(value, paths, texts, format!("{path}/{index}"));
                }
            }
            Value::Object(values) => {
                for (key, value) in values {
                    collect(
                        value,
                        paths,
                        texts,
                        format!("{path}/{}", key.replace('~', "~0").replace('/', "~1")),
                    );
                }
            }
            _ => {}
        }
    }
    let mut value = value;
    if value["method"]
        .as_str()
        .is_some_and(|m| m.ends_with("/delta"))
    {
        return Ok(sanitize(value, &[]));
    }
    let (mut paths, mut texts) = (vec![], vec![]);
    collect(&value, &mut paths, &mut texts, String::new());
    if texts.len() > 32_768 {
        return Err("Event contains too many strings.".into());
    }
    // A bounded history page can contain hundreds of individually small
    // events. Bound each scanner batch without rejecting a normal page.
    let mut redacted = Vec::with_capacity(texts.len());
    for batch in texts.chunks(2048) {
        redacted.extend(
            scanner
                .redact(batch, tokio::sync::watch::channel(false).1)
                .await?,
        );
    }
    for (path, text) in paths.into_iter().zip(redacted) {
        *value
            .pointer_mut(&path)
            .ok_or("Invalid event display path.")? = Value::String(text);
    }
    // Scan the original bounded frame. Truncating first could split a token
    // into a prefix that the full-content detector no longer recognizes.
    Ok(sanitize(value, &[]))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drops_partial_secrets_and_masks_final_text() {
        let delta = sanitize(
            json!({"method":"item/delta","params":{"itemId":"i","delta":"first-half"}}),
            &[],
        );
        assert!(delta["params"].get("delta").is_none());
        assert_eq!(delta["params"]["deltaBytes"], 10);
        let frame = sanitize(
            json!({"method":"item/completed","params":{"text":"first-halfsecond-half"}}),
            &["first-halfsecond-half".into()],
        );
        assert_eq!(frame["params"]["text"], "[redacted]");
    }
    #[test]
    fn large_japanese_text_has_valid_utf8_and_a_visible_truncation_marker() {
        let frame = sanitize(json!({"params":{"text":"猫".repeat(MAX_TEXT)}}), &[]);
        assert!(frame["params"]["text"].as_str().unwrap().len() < MAX_TEXT);
        assert_eq!(frame["_intercatTruncated"][0], "/params/text");
    }
}
