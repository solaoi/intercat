//! Conservative prefixes of the pinned Gitleaks rules. No entropy thresholds or
//! allowlists are applied to unfinished text. A partial match holds its entire
//! prefix, including multiline keys and credentials, until more text arrives.
use pcre2_sys::*;
use serde::Deserialize;
use std::{
    ptr::NonNull,
    sync::OnceLock,
    time::{Duration, Instant},
};

pub(crate) struct Scanned {
    pub cut: usize,
    pub ranges: Vec<(usize, usize, String)>,
}

struct Code(NonNull<pcre2_code_8>);
// PCRE2 compiled patterns are immutable. Each scan owns its match data,
// context and DFA workspace; no JIT compilation or shared mutation occurs.
unsafe impl Send for Code {}
unsafe impl Sync for Code {}
impl Drop for Code {
    fn drop(&mut self) {
        unsafe { pcre2_code_free_8(self.0.as_ptr()) }
    }
}
struct MatchData(NonNull<pcre2_match_data_8>);
impl Drop for MatchData {
    fn drop(&mut self) {
        unsafe { pcre2_match_data_free_8(self.0.as_ptr()) }
    }
}
struct Context(NonNull<pcre2_match_context_8>);
impl Drop for Context {
    fn drop(&mut self) {
        unsafe { pcre2_match_context_free_8(self.0.as_ptr()) }
    }
}
#[derive(Deserialize)]
struct Config {
    rules: Vec<Rule>,
}
#[derive(Deserialize)]
struct Rule {
    regex: Option<String>,
}

fn policy() -> Result<&'static [Code], &'static str> {
    static POLICY: OnceLock<Result<Vec<Code>, &'static str>> = OnceLock::new();
    POLICY
        .get_or_init(|| {
            let config: Config = toml::from_str(crate::secret_scan::CONFIG)
                .map_err(|_| "Invalid pinned secret rules.")?;
            config
                .rules
                .into_iter()
                .filter_map(|rule| rule.regex)
                .map(|pattern| {
                    let (mut error, mut offset) = (0, 0);
                    // Go's shorthand character classes are ASCII; deliberately omit
                    // UCP. UTF enables Unicode literals/case folding. Do not optimize
                    // away alternate lengths, since every possible overlap matters.
                    let code = unsafe {
                        pcre2_compile_8(
                            pattern.as_ptr(),
                            pattern.len(),
                            PCRE2_UTF | PCRE2_NO_AUTO_POSSESS,
                            &mut error,
                            &mut offset,
                            std::ptr::null_mut(),
                        )
                    };
                    NonNull::new(code)
                        .map(Code)
                        .ok_or("Unsupported pinned secret rule.")
                })
                .collect()
        })
        .as_deref()
        .map_err(|error| *error)
}

pub(crate) fn scan(text: &str) -> Result<Scanned, &'static str> {
    if text.len() > 64 * 1024 {
        return Err("Secret preview input limit.");
    }
    let codes = policy()?;
    let data = MatchData(
        NonNull::new(unsafe { pcre2_match_data_create_8(32, std::ptr::null_mut()) })
            .ok_or("Secret preview allocation failed.")?,
    );
    let context = Context(
        NonNull::new(unsafe { pcre2_match_context_create_8(std::ptr::null_mut()) })
            .ok_or("Secret preview allocation failed.")?,
    );
    unsafe {
        pcre2_set_match_limit_8(context.0.as_ptr(), 1_000_000);
        pcre2_set_depth_limit_8(context.0.as_ptr(), 256);
        pcre2_set_heap_limit_8(context.0.as_ptr(), 1024);
    }
    let mut workspace = vec![0; 65_536];
    let mut result = Scanned {
        cut: text.len(),
        ranges: Vec::new(),
    };
    let deadline = Instant::now() + Duration::from_millis(500);
    for code in codes {
        let mut offset = 0;
        while offset < text.len() {
            if Instant::now() >= deadline {
                return Err("Secret preview time limit.");
            }
            // HARD partial with DFA explores all alternatives: even a full
            // shorter match cannot release a prefix of a longer possible one.
            let status = unsafe {
                pcre2_dfa_match_8(
                    code.0.as_ptr(),
                    text.as_ptr(),
                    text.len(),
                    offset,
                    PCRE2_PARTIAL_HARD,
                    data.0.as_ptr(),
                    context.0.as_ptr(),
                    workspace.as_mut_ptr(),
                    workspace.len(),
                )
            };
            if status == PCRE2_ERROR_NOMATCH {
                break;
            }
            if status != PCRE2_ERROR_PARTIAL && status < 0 {
                return Err("Secret preview matching failed.");
            }
            let spans = unsafe { pcre2_get_ovector_pointer_8(data.0.as_ptr()) };
            let (start, end) = unsafe { (*spans, *spans.add(1)) };
            if start < offset
                || end > text.len()
                || start > end
                || (start == end && status != PCRE2_ERROR_PARTIAL)
                || !text.is_char_boundary(start)
                || !text.is_char_boundary(end)
            {
                return Err("Invalid secret preview match.");
            }
            if status == PCRE2_ERROR_PARTIAL {
                result.cut = result.cut.min(start);
                break;
            }
            result.ranges.push((start, end, String::new()));
            if result.ranges.len() > 2048 {
                return Err("Secret preview match limit.");
            }
            // Keep context from the original beginning; restart after the
            // first character of the match to catch overlapping credentials.
            offset = start + text[start..].chars().next().unwrap().len_utf8();
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_pinned_rules_compile_and_ordinary_japanese_advances() {
        assert_eq!(policy().unwrap().len(), 221);
        let text = "検証を開始しました。次に結果を確認しています。\n";
        let result = scan(text).unwrap();
        assert!(result.cut > 30);
        assert!(result.ranges.is_empty());
    }
    #[test]
    fn every_split_of_tokens_and_multiline_keys_holds_the_sensitive_prefix() {
        for secret in [
            format!("ghp_{}", "A6k9P2mR".repeat(4) + "X4v7"),
            format!(
                "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----",
                "A6k9P2mR".repeat(8)
            ),
            format!(
                "curl -H 'Authorization: Bearer {}\n{}' https://example.test",
                "A6k9P2mR".repeat(4),
                "X4v7".repeat(8)
            ),
        ] {
            let text = format!("安全です。\n{secret}\n完了しました。\n");
            let start = "安全です。\n".len();
            for end in start + 1..=start + secret.len() {
                let result = scan(&text[..end]).unwrap();
                let visible = &text[..result.cut];
                assert!(
                    result.cut <= start
                        || result
                            .ranges
                            .iter()
                            .any(|(a, b, _)| *a <= start && *b >= end),
                    "end={end} cut={} visible={visible:?}",
                    result.cut
                );
            }
        }
    }
}
