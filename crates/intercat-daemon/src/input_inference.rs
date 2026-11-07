//! Compare resubmitted requests outside the journal writer.
use crate::{
    Result,
    clock::Stamp,
    decision::Decision,
    journal::Journal,
    request_sources::{Role, Source},
};
use rapidfuzz::distance::levenshtein;
use serde::{Deserialize, Serialize};
use tokio::{sync::watch, task::JoinHandle};
use unicode_normalization::UnicodeNormalization;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Text {
    pub text: String,
    pub sources: Vec<Source>,
}

impl Text {
    async fn resolve(&self, journal: &Journal) -> Result<String> {
        let mut text = self.text.clone();
        for source in &self.sources {
            if source.role == Role::Request {
                text.push_str(&journal.read_pasted_text(source.attachment_id).await?);
            }
            text.push_str(&source.after);
            if text.len() > crate::attachment_store::FILE_LIMIT {
                return Err("The recorded request exceeds its text limit.".into());
            }
        }
        Ok(text)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TakenBack {
    pub input_id: String,
    pub revision: i64,
    pub session_id: String,
    pub text: Text,
    pub decision: Decision,
    pub at: Stamp,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub sequence: i64,
    pub original: TakenBack,
}

#[derive(Clone, Debug)]
pub struct Ticket {
    pub input: String,
    pub revision: i64,
    pub text: Text,
    pub decision: Decision,
    pub put_at: Stamp,
    pub candidates: Vec<Candidate>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Match {
    pub sequence: i64,
    pub edits: usize,
    pub characters: usize,
}

#[derive(Clone, Debug)]
pub enum Evaluation {
    Matched(Match),
    Unmatched,
    Unavailable,
}

fn normalize(text: &str) -> Vec<char> {
    let normalized: String = text.nfkc().flat_map(char::to_lowercase).collect();
    normalized
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .collect()
}

fn compare(current: &str, candidates: Vec<(i64, String)>) -> Option<Match> {
    let current = normalize(current);
    if current.is_empty() {
        return None;
    }
    let mut best: Option<Match> = None;
    for (sequence, text) in candidates {
        let text = normalize(&text);
        if text.is_empty() {
            continue;
        }
        let characters = text.len().max(current.len());
        let Some(edits) = levenshtein::distance_with_args(
            text.iter().copied(),
            current.iter().copied(),
            &levenshtein::Args::default().score_cutoff(characters / 5),
        ) else {
            continue;
        };
        if best.as_ref().is_none_or(|old| {
            (edits as u128) * (old.characters as u128) < (old.edits as u128) * (characters as u128)
        }) {
            best = Some(Match {
                sequence,
                edits,
                characters,
            });
        }
    }
    best
}

async fn evaluate(journal: &Journal, ticket: &Ticket) -> Result<Option<Match>> {
    if ticket.candidates.is_empty() {
        return Ok(None);
    }
    let current = ticket.text.resolve(journal).await?;
    let mut candidates = Vec::new();
    for candidate in &ticket.candidates {
        candidates.push((
            candidate.sequence,
            candidate.original.text.resolve(journal).await?,
        ));
    }
    tokio::task::spawn_blocking(move || compare(&current, candidates))
        .await
        .map_err(Into::into)
}

pub struct Service {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Service {
    pub fn start(journal: Journal) -> Self {
        let (stop, mut stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut changes = journal.subscribe();
            let mut pending = None;
            loop {
                if *stopping.borrow() {
                    return;
                }
                if pending.is_none() {
                    changes.borrow_and_update();
                    if let Ok(Some(ticket)) = journal.next_inferred_correction().await {
                        let result = tokio::select! {
                            _ = stopping.changed() => return,
                            result = evaluate(&journal, &ticket) => result,
                        };
                        let evaluation = match result {
                            Ok(Some(matched)) => Evaluation::Matched(matched),
                            Ok(None) => Evaluation::Unmatched,
                            Err(_) => Evaluation::Unavailable,
                        };
                        pending = Some((ticket, evaluation));
                    }
                }
                if let Some((ticket, result)) = &pending
                    && journal
                        .finish_inferred_correction(ticket.clone(), result.clone())
                        .await
                        .is_ok()
                {
                    pending = None;
                    continue;
                }
                tokio::select! {
                    _ = stopping.changed() => return,
                    _ = changes.changed() => {},
                    _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {},
                }
            }
        });
        Self { stop, task }
    }

    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_unicode_requests_are_compared_without_shortening() {
        let prefix = "資料".repeat(100_000);
        let first = format!("{prefix}甲");
        let second = format!("{prefix}乙");
        let matched = compare(&first, vec![(1, second)]).unwrap();
        assert_eq!(matched.edits, 1);
        assert_eq!(matched.characters, 200_001);
    }

    #[test]
    fn normalization_preserves_content_and_the_twenty_percent_boundary() {
        assert_eq!(
            normalize("  ＣＡＦÉ\n\tカタカナ  "),
            normalize("cafe\u{301} カタカナ")
        );
        assert_eq!(
            compare("abcde", vec![(1, "abcdf".into())]).unwrap().edits,
            1
        );
        assert!(compare("abcde", vec![(1, "abcxy".into())]).is_none());
        assert!(compare("  ", vec![(1, "\n".into())]).is_none());
        assert_eq!(
            compare(
                "日本語でテストします",
                vec![(1, "日本語をテストします".into())]
            )
            .unwrap()
            .edits,
            1
        );
        assert_eq!(
            compare(
                "Keep it",
                vec![(2, "Keep it".into()), (1, "Keep it".into())]
            )
            .unwrap()
            .sequence,
            2
        );
    }
}
