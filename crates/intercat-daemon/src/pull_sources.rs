//! PR descriptions use the committed script and assumptions, never a worktree.
use crate::{Result, pull_request::Sources, push_review::Member};
use rusqlite::{Connection, params};
use std::{collections::BTreeSet, sync::atomic::AtomicBool};
pub(crate) fn capture(db: &Connection, session: &str, member: &Member) -> Result<Sources> {
    let repo = crate::push_review::repository(member)?;
    let included = repo
        .push_commits(
            member.target.sha.parse()?,
            &[member.folder.base.parse()?],
            &AtomicBool::new(false),
        )?
        .into_iter()
        .map(|sha| sha.to_string())
        .collect::<BTreeSet<_>>();
    let mut commits = Vec::new();
    let mut tracks = BTreeSet::new();
    let mut assumptions = Vec::new();
    let mut seen_assumptions = BTreeSet::new();
    let mut cutoff = 0;
    let records=db.prepare("SELECT id FROM integration_jobs WHERE session=?1 AND json_extract(record,'$.project.id')=?2 AND json_extract(record,'$.status')='done' ORDER BY rowid")?.query_map(params![session,member.folder.id],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for id in records {
        let job = crate::integration_store::read(db, id.parse()?)?;
        let Some(candidate) = &job.candidate else {
            continue;
        };
        let undo_selected = candidate
            .undo
            .as_ref()
            .is_some_and(|undo| included.contains(&undo.sha));
        if undo_selected {
            commits.push(candidate.undo.as_ref().unwrap().message.clone());
            if let Some(plan) = &job.undo {
                tracks.extend(plan.records.iter().map(|record| record.track_id.clone()));
            }
        }
        let mut selected = BTreeSet::new();
        for commit in &candidate.commits {
            if included.contains(&commit.sha) {
                commits.push(commit.message.clone());
                tracks.insert(commit.track.clone());
                selected.insert(commit.track.clone());
            }
        }
        if selected.is_empty() && !undo_selected {
            continue;
        }
        if included.contains(&candidate.head)
            && let Some(check) = &candidate.check
        {
            commits.push(format!(
                "Recorded validation at {}: command={}, exit={:?}, truncated={}",
                candidate.head,
                check.command.as_deref().unwrap_or("not configured"),
                check.code,
                check.truncated
            ));
        }
        let published:i64=db.query_row("SELECT coalesce(max(ts),0) FROM events WHERE kind='integration.published' AND json_extract(payload,'$.id')=?1 AND json_extract(payload,'$.status')='done'",[id],|row|row.get(0))?;
        cutoff = cutoff.max(published);
        for review in job.assumptions {
            if review.eligible
                && selected.contains(&review.assumption.track_id)
                && seen_assumptions.insert(review.assumption.id.clone())
            {
                assumptions.push(format!(
                    "{}: {} ({}).",
                    review.assumption.question, review.assumption.text, review.assumption.state
                ));
            }
        }
    }
    if commits.is_empty() {
        return Err("No committed script is available for this PR.".into());
    }
    let mut requests = Vec::new();
    let mut query=db.prepare("SELECT u.text,coalesce(json_extract(u.decision,'$.trackId'),u.provisional) FROM utterances u WHERE u.created_at<=?2 AND u.state IN ('landed','sent') AND EXISTS(SELECT 1 FROM events e WHERE e.kind='utterance.staged' AND json_extract(e.payload,'$.id')=u.id AND json_extract(e.payload,'$.sessionId')=?1) ORDER BY u.created_at,u.id")?;
    for row in query.query_map(params![session, cutoff], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })? {
        let (text, track) = row?;
        if track.as_ref().is_some_and(|track| tracks.contains(track)) {
            requests.push(text);
        }
    }
    let sources = Sources {
        requests,
        commits,
        assumptions,
    };
    if serde_json::to_vec(&sources)?.len() > 512 * 1024 {
        return Err(
            "The committed script is too large for one PR description. Review a smaller range."
                .into(),
        );
    }
    Ok(sources)
}
