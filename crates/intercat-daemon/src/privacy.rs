//! Stable local identities for values removed from outgoing text.
//! This catalog is separate from fingerprint-only secret-input credentials.
use crate::{Result, secrets::Key};
use regex::Regex;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::OnceLock};

const INPUT_LIMIT: usize = 64 * 1024 * 1024;
const FINDING_LIMIT: usize = 2048;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Kind {
    ApiKey,
    Token,
    Email,
    Phone,
    Password,
    Name,
    Address,
}
impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::ApiKey => "api-key",
            Self::Token => "token",
            Self::Email => "email",
            Self::Phone => "phone",
            Self::Password => "password",
            Self::Name => "name",
            Self::Address => "address",
        }
    }
}
#[derive(Clone)]
pub(crate) struct Span {
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
}
pub(crate) struct Prepared {
    texts: Vec<String>,
    spans: Vec<Vec<Span>>,
}

impl Prepared {
    pub(crate) fn full_texts(&self) -> Vec<String> {
        self.texts
            .iter()
            .zip(&self.spans)
            .map(|(text, spans)| {
                let mut safe = text.clone();
                for span in spans.iter().rev() {
                    safe.replace_range(span.start..span.end, &" ".repeat(span.end - span.start));
                }
                safe
            })
            .collect()
    }
    pub(crate) fn extend(&mut self, extra: Vec<Vec<Span>>) -> Result<()> {
        if extra.len() != self.texts.len() {
            return Err("Incomplete Full privacy result.".into());
        }
        for ((text, spans), extra) in self.texts.iter().zip(&mut self.spans).zip(extra) {
            for span in &extra {
                if span.start >= span.end || text.get(span.start..span.end).is_none() {
                    return Err("Invalid Full privacy range.".into());
                }
            }
            spans.extend(extra);
            spans.sort_by_key(|span| (span.start, std::cmp::Reverse(span.end), span.kind));
            let mut merged: Vec<Span> = Vec::new();
            for span in spans.drain(..) {
                if let Some(previous) = merged.last_mut()
                    && span.start < previous.end
                {
                    previous.end = previous.end.max(span.end);
                } else {
                    merged.push(span);
                }
            }
            *spans = merged;
        }
        if self.spans.iter().map(Vec::len).sum::<usize>() > FINDING_LIMIT {
            return Err("Too many privacy findings; no partial result was accepted.".into());
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Part {
    Text { text: String },
    Redaction { id: i64, category: String },
}
#[derive(Serialize, Debug)]
pub struct View {
    pub text: String,
    pub parts: Vec<Part>,
}

fn patterns() -> &'static [(Regex, Kind)] {
    static PATTERNS: OnceLock<Vec<(Regex, Kind)>> = OnceLock::new();
    PATTERNS.get_or_init(|| vec![
        (Regex::new(r"(?i)[a-z0-9.!#$%&'*+/=?^_`{|}~-]+@[a-z0-9](?:[a-z0-9-]*[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]*[a-z0-9])?)+").unwrap(), Kind::Email),
        (Regex::new(r"[0０](?:[0-9０-９]{1,4}[-‐‑‒–—―−ー 　][0-9０-９]{1,4}[-‐‑‒–—―−ー 　][0-9０-９]{3,4}|[0-9０-９]{9,10})").unwrap(), Kind::Phone),
        (Regex::new(r"[0０][0-9０-９]{1,4}[(（][0-9０-９]{1,4}[)）][-‐‑‒–—―−ー 　]?[0-9０-９]{3,4}").unwrap(), Kind::Phone),
        (Regex::new(r"[+＋][1-9１-９][0-9０-９]{0,2}(?:[ ()（）‐‑‒–—―−-]*[0-9０-９]){7,14}").unwrap(), Kind::Phone),
        (Regex::new(r"(?:\([2-9][0-9]{2}\)|[2-9][0-9]{2})[ -][0-9]{3}[ -][0-9]{4}").unwrap(), Kind::Phone),
    ])
}

pub(crate) async fn prepare(texts: Vec<String>, directory: &Path) -> Result<Prepared> {
    if texts.iter().any(|text| text.contains('\0')) {
        return Err("Privacy scan cannot inspect text containing NUL bytes.".into());
    }
    if texts
        .iter()
        .try_fold(0usize, |total, text| total.checked_add(text.len()))
        .is_none_or(|total| total > INPUT_LIMIT)
    {
        return Err("Privacy scan input exceeds 64 MiB.".into());
    }
    let directory = directory.to_path_buf();
    let scanner = tokio::task::spawn_blocking(move || {
        crate::secret_scan::Engine::capture()?.start_in(&directory, &[])
    })
    .await??;
    let mut spans = scanner.private_spans(&texts).await?;
    let (texts, native) = tokio::task::spawn_blocking(move || -> Result<_> {
        let native = crate::native_privacy::phones(&texts)?;
        Ok((texts, native))
    })
    .await??;
    for ((text, matches), native) in texts.iter().zip(&mut spans).zip(native) {
        matches.extend(native);
        for (pattern, kind) in patterns() {
            for found in pattern.find_iter(text) {
                if *kind == Kind::Phone
                    && (text[..found.start()]
                        .chars()
                        .next_back()
                        .is_some_and(|c| c.is_ascii_alphanumeric() || ('０'..='９').contains(&c))
                        || text[found.end()..].chars().next().is_some_and(|c| {
                            c.is_ascii_alphanumeric() || ('０'..='９').contains(&c)
                        }))
                {
                    continue;
                }
                if *kind == Kind::Phone {
                    let digits = found
                        .as_str()
                        .chars()
                        .filter(|c| c.is_ascii_digit() || ('０'..='９').contains(c))
                        .count();
                    let international = found.as_str().starts_with(['+', '＋']);
                    if (!international && ![10, 11].contains(&digits))
                        || (international && !(8..=15).contains(&digits))
                    {
                        continue;
                    }
                }
                matches.push(Span {
                    start: found.start(),
                    end: found.end(),
                    kind: *kind,
                });
                if matches.len() > FINDING_LIMIT {
                    return Err("Too many privacy findings; no partial result was accepted.".into());
                }
            }
        }
        matches.sort_by_key(|span| (span.start, std::cmp::Reverse(span.end), span.kind));
        let mut merged: Vec<Span> = Vec::new();
        for span in matches.drain(..) {
            if let Some(previous) = merged.last_mut()
                && span.start < previous.end
            {
                previous.end = previous.end.max(span.end);
            } else {
                merged.push(span);
            }
        }
        *matches = merged;
    }
    if spans.iter().map(Vec::len).sum::<usize>() > FINDING_LIMIT {
        return Err("Too many privacy findings; no partial result was accepted.".into());
    }
    Ok(Prepared { texts, spans })
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS privacy_values(id INTEGER PRIMARY KEY AUTOINCREMENT,tag BLOB NOT NULL UNIQUE CHECK(length(tag)=32),kind TEXT NOT NULL,value TEXT NOT NULL);")?;
    Ok(())
}

pub(crate) fn apply(db: &mut Connection, key: &Key, prepared: Prepared) -> Result<Vec<View>> {
    let tx = db.transaction()?;
    let mut output = Vec::with_capacity(prepared.texts.len());
    for (text, spans) in prepared.texts.iter().zip(&prepared.spans) {
        let mut safe = String::new();
        let mut position = 0;
        for span in spans {
            let value = text
                .get(span.start..span.end)
                .filter(|value| !value.is_empty())
                .ok_or("Invalid privacy span.")?;
            let mut source = b"intercat.privacy.value.v1\0".to_vec();
            source.extend_from_slice(value.as_bytes());
            let tag = key.tag(&source);
            let existing: Option<(i64, String, String)> = tx
                .query_row(
                    "SELECT id,kind,value FROM privacy_values WHERE tag=?1",
                    [tag.as_slice()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let (id, kind, saved) = if let Some(existing) = existing {
                existing
            } else {
                tx.execute(
                    "INSERT INTO privacy_values(tag,kind,value) VALUES(?1,?2,?3)",
                    params![tag.as_slice(), span.kind.name(), value],
                )?;
                (
                    tx.last_insert_rowid(),
                    span.kind.name().to_owned(),
                    value.to_owned(),
                )
            };
            if saved != value {
                return Err("The local privacy value does not match its identity.".into());
            }
            let plain = text
                .get(position..span.start)
                .ok_or("Overlapping privacy spans.")?;
            safe.push_str(plain);
            if ![
                "api-key", "token", "email", "phone", "password", "name", "address",
            ]
            .contains(&kind.as_str())
            {
                return Err("Invalid saved privacy category.".into());
            }
            safe.push_str(&format!("<redacted:{kind}#{id}>"));
            position = span.end;
        }
        let plain = &text[position..];
        safe.push_str(plain);
        output.push(view(&tx, safe)?);
    }
    tx.commit()?;
    Ok(output)
}

fn view(db: &Connection, text: String) -> Result<View> {
    static LABEL: OnceLock<Regex> = OnceLock::new();
    let labels = LABEL.get_or_init(|| {
        Regex::new(r"<redacted:(api-key|token|email|phone|password|name|address)#([1-9][0-9]*)>")
            .unwrap()
    });
    let mut parts = Vec::new();
    let mut previous = 0;
    for found in labels.captures_iter(&text) {
        let Ok(id) = found[2].parse::<i64>() else {
            continue;
        };
        let saved: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM privacy_values WHERE id=?1 AND kind=?2)",
            params![id, &found[1]],
            |row| row.get(0),
        )?;
        if !saved {
            continue;
        }
        let label = found.get(0).unwrap();
        if previous < label.start() {
            parts.push(Part::Text {
                text: text[previous..label.start()].to_owned(),
            });
        }
        parts.push(Part::Redaction {
            id,
            category: found[1].to_owned(),
        });
        previous = label.end();
    }
    if previous < text.len() {
        parts.push(Part::Text {
            text: text[previous..].to_owned(),
        });
    }
    Ok(View { text, parts })
}

pub(crate) fn original(db: &Connection, id: i64) -> Result<String> {
    if id <= 0 {
        return Err("Invalid privacy value.".into());
    }
    db.query_row(
        "SELECT value FROM privacy_values WHERE id=?1",
        [id],
        |row| row.get(0),
    )
    .optional()?
    .ok_or_else(|| "This privacy value is unavailable.".into())
}

/// A replacement occupies one source value, even when its visible label is longer.
pub(crate) fn display_length(text: &str) -> usize {
    static LABEL: OnceLock<Regex> = OnceLock::new();
    let labels = LABEL
        .get_or_init(|| Regex::new(r"<(?:redacted:[a-z-]+#[0-9]+|secret:[A-Z0-9_]+)>").unwrap());
    let mut length = 0;
    let mut previous = 0;
    for label in labels.find_iter(text) {
        length += text[previous..label.start()].chars().count() + 1;
        previous = label.end();
    }
    length + text[previous..].chars().count()
}

pub(crate) fn truncate(text: &str, characters: usize) -> String {
    let Some((mut end, _)) = text.char_indices().nth(characters) else {
        return text.to_owned();
    };
    if let Some(start) = text[..end].rfind('<') {
        let suffix = &text[start..];
        if (suffix.starts_with("<redacted:") || suffix.starts_with("<secret:"))
            && let Some(close) = suffix.find('>')
            && close < 192
            && start + close >= end
        {
            end = start + close + 1;
        }
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(texts: &[&str]) -> Prepared {
        Prepared {
            texts: texts.iter().map(|text| (*text).to_owned()).collect(),
            spans: texts
                .iter()
                .map(|text| {
                    vec![Span {
                        start: 0,
                        end: text.len(),
                        kind: Kind::Email,
                    }]
                })
                .collect(),
        }
    }

    fn rendered(result: Result<Vec<View>>) -> Result<Vec<String>> {
        Ok(result?.into_iter().map(|view| view.text).collect())
    }

    #[test]
    fn stored_labels_remain_interactive_without_treating_unknown_ids_as_values() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        install(&db)?;
        let key = Key::new([7; 32]);
        let original = apply(&mut db, &key, prepared(&["dummy@example.invalid"]))?;
        let text = format!(
            "{} <redacted:phone#1> <redacted:email#999>",
            original[0].text
        );
        let rendered = apply(
            &mut db,
            &key,
            Prepared {
                texts: vec![text],
                spans: vec![vec![]],
            },
        )?;
        assert!(
            matches!(&rendered[0].parts[0], Part::Redaction { id: 1, category } if category == "email")
        );
        assert!(
            matches!(&rendered[0].parts[1], Part::Text { text } if text == " <redacted:phone#1> <redacted:email#999>")
        );
        assert_eq!(original[0].text, "<redacted:email#1>");
        Ok(())
    }

    #[test]
    fn catalog_reuses_identity_without_consuming_numbers() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        install(&db)?;
        let key = Key::new([7; 32]);
        assert_eq!(
            rendered(apply(
                &mut db,
                &key,
                prepared(&[
                    "one@example.invalid",
                    "one@example.invalid",
                    "two@example.invalid"
                ])
            ))?,
            vec![
                "<redacted:email#1>",
                "<redacted:email#1>",
                "<redacted:email#2>"
            ]
        );
        let mut alternate = prepared(&["one@example.invalid"]);
        alternate.spans[0][0].kind = Kind::Token;
        assert_eq!(
            rendered(apply(&mut db, &key, alternate))?,
            vec!["<redacted:email#1>"]
        );
        assert_eq!(original(&db, 2)?, "two@example.invalid");
        Ok(())
    }

    #[test]
    fn full_database_rolls_back_the_whole_replacement_batch() -> Result<()> {
        let mut db = Connection::open_in_memory()?;
        db.execute_batch("PRAGMA page_size=512;")?;
        install(&db)?;
        let pages: i64 = db.query_row("PRAGMA page_count", [], |row| row.get(0))?;
        db.pragma_update(None, "max_page_count", pages)?;
        let large = "x".repeat(8192);
        let failure = apply(
            &mut db,
            &Key::new([7; 32]),
            prepared(&["first@example.invalid", &large]),
        )
        .unwrap_err();
        assert!(
            matches!(failure.downcast_ref::<rusqlite::Error>(),Some(rusqlite::Error::SqliteFailure(code,_)) if code.code == rusqlite::ErrorCode::DiskFull)
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM privacy_values", [], |row| row
                .get::<_, i64>(0))?,
            0
        );
        db.pragma_update(None, "max_page_count", pages + 100)?;
        assert_eq!(
            rendered(apply(
                &mut db,
                &Key::new([7; 32]),
                prepared(&["first@example.invalid"])
            ))?,
            vec!["<redacted:email#1>"]
        );
        Ok(())
    }
    #[test]
    fn short_titles_keep_complete_replacement_labels() {
        assert_eq!(
            truncate("prefix <redacted:email#12> suffix", 10),
            "prefix <redacted:email#12>"
        );
        assert_eq!(
            truncate("先頭<secret:API_TOKEN>末尾", 4),
            "先頭<secret:API_TOKEN>"
        );
        assert_eq!(truncate("plain text", 5), "plain");
        assert_eq!(truncate("<redacted:phone#1>", 0), "");
    }
}
