//! A persisted, per-window lease keeps an unsent request out of dispatch.
use crate::{
    Result,
    clock::Stamp,
    decision::{Decision, Kind, Outcome, Target},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

const KIND: &str = "utterance.edit";
const TIMEOUT_MS: i64 = 30_000;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub owner: Uuid,
    pub action: Action,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Action {
    Begin {
        token: Uuid,
        #[serde(rename = "utteranceId")]
        input: String,
        revision: i64,
    },
    Renew {
        token: Uuid,
    },
    Apply {
        #[serde(rename = "opId")]
        op_id: Uuid,
        token: Uuid,
        revision: i64,
        edit: Edit,
    },
    End {
        token: Uuid,
        revision: i64,
        edit: Edit,
    },
    Disconnect,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Edit {
    Cancel,
    TakeBack,
    TryTwoWays {
        #[serde(rename = "opId")]
        op_id: Uuid,
        selection: crate::two_ways::Selection,
    },
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        sources: Option<Vec<crate::request_sources::Source>>,
    },
    WaitFor {
        source: crate::start_condition::Target,
    },
    ReadAs {
        #[serde(rename = "trackId")]
        track: String,
        boundary: Boundary,
        turn: Option<crate::decision_review::Turn>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Boundary {
    Now,
    AfterTurn,
    WhenTold,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Lease {
    owner: Uuid,
    token: Uuid,
    input: String,
    revision: i64,
    renewed: Stamp,
    edit: Option<Edit>,
}

fn leases(db: &Connection, state: &str) -> Result<Vec<Lease>> {
    db.prepare("SELECT payload FROM operations WHERE kind=?1 AND state=?2 ORDER BY rowid")?
        .query_map(params![KIND, state], |row| row.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect()
}

fn save(db: &Connection, lease: &Lease, state: &str) -> Result<()> {
    db.execute(
        "UPDATE operations SET payload=?2,state=?3 WHERE id=?1 AND kind=?4",
        params![
            lease.token.to_string(),
            serde_json::to_string(lease)?,
            state,
            KIND
        ],
    )?;
    Ok(())
}

fn event(db: &Connection, lease: &Lease, state: &str) -> Result<()> {
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.edit',?2)",
        params![
            crate::journal::timestamp(),
            json!({"id":lease.input,"token":lease.token,"state":state}).to_string()
        ],
    )?;
    Ok(())
}

pub fn active(db: &Connection, input: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE kind=?1 AND state='prepared' AND json_extract(payload,'$.input')=?2)",
        params![KIND, input], |row| row.get(0))?)
}

pub fn status(db: &Connection, input: &str) -> Result<Option<Value>> {
    db.query_row("SELECT payload FROM operations WHERE kind=?1 AND state='prepared' AND json_extract(payload,'$.input')=?2",
        params![KIND, input], |row| row.get::<_, String>(0)).optional()?
        .map(|text| {
            let lease: Lease = serde_json::from_str(&text)?;
            Ok(json!({"owner":lease.owner,"token":lease.token,"revision":lease.revision}))
        }).transpose()
}

fn interrupt(db: &Connection, lease: &Lease) -> Result<()> {
    db.execute("UPDATE utterances SET state='held',revision=revision+1,reason='editing was interrupted' WHERE id=?1 AND revision=?2",
        params![lease.input, lease.revision])?;
    crate::redirect::input_state(db, &lease.input, "held")?;
    save(db, lease, "superseded")?;
    event(db, lease, "interrupted")
}

pub fn expire(db: &mut Connection, now: &Stamp) -> Result<()> {
    let expired: Vec<_> = leases(db, "prepared")?
        .into_iter()
        .filter(|lease| {
            now.boot_id != lease.renewed.boot_id || now.elapsed(&lease.renewed).0 >= TIMEOUT_MS
        })
        .collect();
    if expired.is_empty() {
        return Ok(());
    }
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    for lease in expired {
        interrupt(&tx, &lease)?;
    }
    tx.commit()?;
    Ok(())
}

pub fn change(db: &mut Connection, request: Request, now: Stamp) -> Result<Option<String>> {
    expire(db, &now)?;
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let input = match request.action {
        Action::Begin {
            token,
            input,
            revision,
        } => {
            if crate::two_ways_store::fenced(&tx, &input)? {
                return Err("Cancel the paired attempt before editing this instruction.".into());
            }
            if crate::review_fix_store::for_input(&tx, &input)?.is_some() {
                return Err("Review fixes keep their reviewed target. Take back the fix or use the review decision.".into());
            }
            if token.get_version_num() != 7 || revision < 1 {
                return Err("Invalid edit lease.".into());
            }
            if let Some((kind, text, state)) = tx
                .query_row(
                    "SELECT kind,payload,state FROM operations WHERE id=?1",
                    [token.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()?
            {
                if kind != KIND {
                    return Err("Edit token reused.".into());
                }
                let lease: Lease = serde_json::from_str(&text)?;
                if lease.owner != request.owner
                    || lease.input != input
                    || lease.revision != revision + 1
                    || state != "prepared"
                {
                    return Err("The edit lease changed. Reopen the request.".into());
                }
                return Ok(Some(input));
            }
            if active(&tx, &input)? {
                return Err("This request is being edited in another window.".into());
            }
            let (state, current): (String, i64) = tx.query_row(
                "SELECT state,revision FROM utterances WHERE id=?1",
                [&input],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if current != revision {
                return Err("This request changed. Reopen it before editing.".into());
            }
            if ![
                "staged",
                "reading",
                "placed",
                "asked",
                "held",
                "redirecting",
                "orphaned",
            ]
            .contains(&state.as_str())
                || crate::redirect::interrupt_started(&tx, &input)?
            {
                return Err("This request was already sent.".into());
            }
            tx.execute("UPDATE utterances SET revision=revision+1,state=CASE WHEN state='reading' THEN 'staged' ELSE state END WHERE id=?1", [&input])?;
            crate::timeline_history::advance_revision(&tx, &input, revision)?;
            crate::redirect::revise(&tx, &input, revision + 1)?;
            let lease = Lease {
                owner: request.owner,
                token,
                input: input.clone(),
                revision: revision + 1,
                renewed: now,
                edit: None,
            };
            tx.execute(
                "INSERT INTO operations VALUES(?1,?2,?3,'prepared')",
                params![token.to_string(), KIND, serde_json::to_string(&lease)?],
            )?;
            event(&tx, &lease, "began")?;
            Some(input)
        }
        Action::Disconnect => {
            for lease in leases(&tx, "prepared")?
                .into_iter()
                .filter(|lease| lease.owner == request.owner)
            {
                interrupt(&tx, &lease)?;
            }
            None
        }
        action => {
            let token = match &action {
                Action::Renew { token }
                | Action::Apply { token, .. }
                | Action::End { token, .. } => *token,
                _ => unreachable!(),
            };
            let (text, state): (String, String) = tx.query_row(
                "SELECT payload,state FROM operations WHERE id=?1 AND kind=?2",
                params![token.to_string(), KIND],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let mut lease: Lease = serde_json::from_str(&text)?;
            // A reopened window may retry only the exact finish already saved
            // with the original lease in its durable composer draft.
            let saved_finish = lease.owner != request.owner
                && matches!(action, Action::End { .. })
                && crate::drafts::contains_pending_edit(
                    &tx,
                    lease.owner,
                    &lease.input,
                    &serde_json::to_value(&action)?,
                )?;
            if lease.owner != request.owner && !saved_finish {
                return Err("This window does not own the edit lease.".into());
            }
            if let Action::End { revision, edit, .. } = &action
                && state == "complete"
                && *revision == lease.revision
                && lease.edit.as_ref() == Some(edit)
            {
                return Ok(Some(lease.input));
            }
            let latest: String = tx.query_row("SELECT id FROM operations WHERE kind=?1 AND json_extract(payload,'$.input')=?2 ORDER BY rowid DESC LIMIT 1",
                params![KIND, lease.input], |row| row.get(0))?;
            if latest != token.to_string() {
                return Err("A newer edit owns this request. Reopen it.".into());
            }
            match action {
                Action::Apply {
                    op_id,
                    revision,
                    edit,
                    ..
                } => {
                    if op_id.get_version_num() != 7
                        || matches!(
                            edit,
                            Edit::Cancel | Edit::TakeBack | Edit::TryTwoWays { .. }
                        )
                    {
                        return Err("Invalid request edit.".into());
                    }
                    let payload =
                        json!({"token":token,"revision":revision,"edit":edit}).to_string();
                    if let Some((kind, saved)) = tx
                        .query_row(
                            "SELECT kind,payload FROM operations WHERE id=?1",
                            [op_id.to_string()],
                            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                        )
                        .optional()?
                    {
                        if kind != "utterance.edit.apply" || saved != payload {
                            return Err("Edit operation ID reused.".into());
                        }
                        return Ok(Some(lease.input));
                    }
                    let expected = lease.revision + i64::from(state == "superseded");
                    let current: i64 = tx.query_row(
                        "SELECT revision FROM utterances WHERE id=?1",
                        [&lease.input],
                        |row| row.get(0),
                    )?;
                    if revision != lease.revision
                        || !["prepared", "superseded"].contains(&state.as_str())
                        || current != expected
                    {
                        return Err("The request changed while it was being edited.".into());
                    }
                    apply_edit(&tx, &lease.input, expected, &edit)?;
                    lease.revision = expected + 1;
                    lease.renewed = now;
                    save(&tx, &lease, "prepared")?;
                    tx.execute(
                        "INSERT INTO operations VALUES(?1,'utterance.edit.apply',?2,'complete')",
                        params![op_id.to_string(), payload],
                    )?;
                    event(&tx, &lease, "changed")?;
                }
                Action::Renew { .. } => {
                    if state != "prepared" {
                        return Err("Editing was interrupted. Save or reopen the request.".into());
                    }
                    lease.renewed = now;
                    save(&tx, &lease, "prepared")?;
                }
                Action::End { revision, edit, .. } => {
                    if revision != lease.revision {
                        return Err("The edit version changed.".into());
                    }
                    if state == "complete" {
                        if lease.edit.as_ref() != Some(&edit) {
                            return Err("The edit was already completed differently.".into());
                        }
                        return Ok(Some(lease.input));
                    }
                    let expected = lease.revision + i64::from(state == "superseded");
                    let current: i64 = tx.query_row(
                        "SELECT revision FROM utterances WHERE id=?1",
                        [&lease.input],
                        |row| row.get(0),
                    )?;
                    if !["prepared", "superseded"].contains(&state.as_str())
                        || (edit != Edit::Cancel && current != expected)
                    {
                        return Err("The request changed while it was being edited.".into());
                    }
                    apply_edit(&tx, &lease.input, expected, &edit)?;
                    lease.edit = Some(edit);
                    save(&tx, &lease, "complete")?;
                    event(&tx, &lease, "ended")?;
                    crate::input_readings::touched(&tx, &lease.input)?;
                }
                _ => unreachable!(),
            }
            Some(lease.input)
        }
    };
    tx.commit()?;
    Ok(input)
}

fn apply_edit(
    db: &rusqlite::Transaction<'_>,
    input: &str,
    revision: i64,
    edit: &Edit,
) -> Result<()> {
    if matches!(
        edit,
        Edit::Text { .. } | Edit::ReadAs { .. } | Edit::WaitFor { .. }
    ) {
        crate::timeline_history::abandon_redo(db)?;
    }
    match edit {
        Edit::Cancel => {}
        Edit::TryTwoWays { op_id, selection } => {
            crate::two_ways_store::prepare(
                db,
                crate::two_ways::Request {
                    op_id: *op_id,
                    input_id: input.into(),
                    revision,
                    selection: selection.clone(),
                },
            )?;
        }
        Edit::TakeBack => {
            crate::journal::take_back(db, input)?;
        }
        Edit::Text { text, sources } => {
            let before = crate::inference_store::text(db, input)?;
            crate::request_sources::edit(db, input, text, sources.as_deref())?;
            crate::redirect::forget(db, input)?;
            crate::report_store::detach_orphaned_reply(db, input)?;
            db.execute("UPDATE utterances SET text=?2,state='staged',revision=revision+1,decision=NULL,reason=NULL WHERE id=?1", params![input, text])?;
            crate::reason_review::capture_sources(db, input)?;
            crate::start_condition::capture(db, input)?;
            crate::app_commands::capture(db, input)?;
            crate::inference_store::reworded(db, input, &before)?;
        }
        Edit::WaitFor { source } => {
            let raw: Option<String> = db.query_row(
                "SELECT decision FROM utterances WHERE id=?1",
                [input],
                |row| row.get(0),
            )?;
            let mut decision: Decision = serde_json::from_str(
                &raw.ok_or("Choose a destination before choosing work to wait for.")?,
            )?;
            if !matches!(decision.kind, Kind::Fork | Kind::Extend | Kind::Schedule)
                || decision.track_id.is_none()
            {
                return Err("Choose a destination before choosing work to wait for.".into());
            }
            crate::start_condition::select(db, input, source)?;
            if decision.kind != Kind::Fork {
                decision.kind = Kind::Schedule;
            }
            decision.boundary = Some("after-turn".into());
            decision.scores = json!({"human":true});
            crate::input_feedback::read_as(db, input, revision, &decision)?;
            crate::redirect::forget(db, input)?;
            crate::report_store::detach_orphaned_reply(db, input)?;
            db.execute(
                "UPDATE utterances SET revision=revision+1 WHERE id=?1",
                [input],
            )?;
            crate::journal::apply_outcome(db, input, revision + 1, Outcome::Classified(decision))?;
        }
        Edit::ReadAs {
            track,
            boundary,
            turn,
        } => {
            if !db.query_row(
                "SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)",
                [track],
                |row| row.get::<_, bool>(0),
            )? {
                return Err("The selected track is unavailable.".into());
            }
            let worker: Option<String> = db
                .query_row(
                    "SELECT record FROM workers WHERE track_id=?1",
                    [track],
                    |row| row.get(0),
                )
                .optional()?;
            let worker = worker
                .map(|text| serde_json::from_str::<crate::runtime::WorkerState>(&text))
                .transpose()?;
            let current = worker.and_then(|worker| {
                Some(crate::decision_review::Turn {
                    session_id: worker.session_id?,
                    turn_id: worker.active_turn?,
                })
            });
            if *boundary == Boundary::Now && current.is_some() && *turn != current {
                return Err(
                    "The worker moved to another turn. Reopen this request before stopping it."
                        .into(),
                );
            }
            if *boundary == Boundary::AfterTurn {
                crate::start_condition::bind_turn(db, input, track, turn.as_ref())?;
            } else {
                crate::start_condition::clear(db, input)?;
            }
            let active = current.is_some();
            let kind = match boundary {
                Boundary::Now if active => Kind::Trim,
                Boundary::Now => Kind::Extend,
                _ => Kind::Schedule,
            };
            let decision = Decision {
                kind,
                track_id: Some(track.clone()),
                boundary: Some(
                    match boundary {
                        Boundary::Now => "now",
                        Boundary::AfterTurn => "after-turn",
                        Boundary::WhenTold => "when-told",
                    }
                    .into(),
                ),
                command: None,
                confidence: 1.0,
                inference_ms: 0.0,
                timed_out: false,
                scores: json!({"human":true}),
            };
            crate::input_feedback::read_as(db, input, revision, &decision)?;
            crate::redirect::forget(db, input)?;
            crate::report_store::detach_orphaned_reply(db, input)?;
            db.execute(
                "UPDATE utterances SET target=?2,revision=revision+1 WHERE id=?1",
                params![input, serde_json::to_string(&Target::Lock(track.clone()))?],
            )?;
            crate::journal::apply_outcome(db, input, revision + 1, Outcome::Classified(decision))?;
        }
    }
    Ok(())
}
