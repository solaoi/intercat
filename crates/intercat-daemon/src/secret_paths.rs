//! Workspace deny globs with exactly the same example-file exceptions as Git.
use std::collections::{BTreeMap, BTreeSet};

pub fn deny_globs() -> BTreeMap<String, &'static str> {
    let mut rules = BTreeMap::from([
        ("**/.env".into(), "deny"),
        ("**/.envrc".into(), "deny"),
        ("**/.netrc".into(), "deny"),
        ("**/*.pem".into(), "deny"),
        ("**/*.key".into(), "deny"),
    ]);
    let examples: Vec<_> = intercat_git::ENV_EXAMPLES
        .iter()
        .map(|name| name.strip_prefix(".env.").unwrap())
        .collect();
    partition("", &examples, &mut rules);
    rules
}

fn partition(prefix: &str, examples: &[&str], rules: &mut BTreeMap<String, &'static str>) {
    let base = format!("**/.env.{prefix}");
    if !examples.contains(&prefix) {
        rules.insert(base.clone(), "deny");
    }
    let next: BTreeSet<_> = examples
        .iter()
        .filter_map(|word| word.strip_prefix(prefix)?.chars().next())
        .collect();
    let Some(&low) = next.first() else {
        rules.insert(format!("{base}?*"), "deny");
        return;
    };
    let high = *next.last().unwrap();
    // The pinned host's native translator mishandles a negated class with
    // several members. One range (or one character) remains exact. Deny the
    // gaps inside that range separately with positive ASCII ranges.
    let range = if low == high {
        low.to_string()
    } else {
        format!("{low}-{high}")
    };
    rules.insert(format!("{base}[!{range}]*"), "deny");
    let letters: Vec<_> = next.into_iter().collect();
    for pair in letters.windows(2) {
        let start = pair[0] as u8 + 1;
        let end = pair[1] as u8 - 1;
        if start <= end {
            let range = if start == end {
                char::from(start).to_string()
            } else {
                format!("{}-{}", char::from(start), char::from(end))
            };
            rules.insert(format!("{base}[{range}]*"), "deny");
        }
    }
    for letter in letters {
        partition(&format!("{prefix}{letter}"), examples, rules);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::bstr::ByteSlice;

    #[test]
    fn deny_globs_match_secret_classification_for_prefixes_suffixes_and_unicode() {
        let rules = deny_globs();
        let mut names: BTreeSet<String> = [
            "plain.txt",
            ".env",
            ".env.",
            ".envrc",
            ".netrc",
            "a.key",
            "a.pem",
        ]
        .map(str::to_owned)
        .into();
        let substitutions: Vec<char> = (1u8..=127)
            .filter(|byte| *byte != b'/')
            .map(char::from)
            .chain([
                'é',
                '\u{301}',
                'đ',
                'ð',
                'ß',
                'ı',
                'Ω',
                '本',
                '😀',
                '\u{e000}',
                '\u{10fffd}',
            ])
            .collect();
        for example in intercat_git::ENV_EXAMPLES {
            let suffix = example.strip_prefix(".env.").unwrap();
            names.insert(example.into());
            for index in 0..=suffix.len() {
                names.insert(format!(".env.{}", &suffix[..index]));
                for letter in &substitutions {
                    names.insert(format!(
                        ".env.{}{letter}{}",
                        &suffix[..index],
                        &suffix[index..]
                    ));
                    if index < suffix.len() {
                        names.insert(format!(
                            ".env.{}{letter}{}",
                            &suffix[..index],
                            &suffix[index + 1..]
                        ));
                    }
                }
            }
        }
        for name in names {
            for parent in ["", "nested/", "nested/deep/"] {
                let path = format!("{parent}{name}");
                let denied = rules.keys().any(|pattern| {
                    gix::glob::wildmatch(
                        pattern.as_bytes().as_bstr(),
                        path.as_bytes().as_bstr(),
                        gix::glob::wildmatch::Mode::NO_MATCH_SLASH_LITERAL,
                    )
                });
                assert_eq!(denied, intercat_git::secret_path(&path), "{path:?}");
            }
        }
    }
}
