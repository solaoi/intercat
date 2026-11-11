//! A per-instruction effort does not change the session's standing default.
use crate::{
    Result,
    branch::Branch,
    two_ways::{Instruction, InstructionPhase, Job},
};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

fn instruction(
    db: &Connection,
    track: &str,
    session: &str,
    active_turn: Option<&str>,
) -> Result<Option<Instruction>> {
    let row = db
        .query_row(
            "SELECT o.payload,b.record,u.state,u.dispatch_command FROM operations o
         JOIN branches b ON b.id=json_extract(o.payload,'$.branchId')
         JOIN utterances u ON u.id=json_extract(o.payload,'$.childInput')
         WHERE o.kind='utterance.two-ways' AND o.state='complete'
           AND json_extract(o.payload,'$.phase')='queued'
           AND json_extract(b.record,'$.phase')='ready'
           AND json_extract(u.decision,'$.trackId')=?1
           AND u.state IN ('placed','held','starting','sent','landed')
         ORDER BY o.rowid DESC LIMIT 1",
            [track],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((raw, branch, state, command)) = row else {
        return Ok(None);
    };
    let job: Job = serde_json::from_str(&raw)?;
    let branch: Branch = serde_json::from_str(&branch)?;
    let effort = job.variation.ok_or("Missing paired effort.")?.alternate;
    let phase = match state.as_str() {
        "placed" | "held" | "starting" => {
            let declared = &branch.track_id;
            let current = crate::continuation::current(db, declared)?;
            if current != track {
                return Ok(None);
            }
            let expected = if current == *declared {
                branch.session_id.ok_or("Missing paired branch session.")?
            } else {
                crate::continuation::start(db, &current)?
                    .ok_or("Missing paired continuation.")?
                    .session_id
            };
            if session != expected {
                return Ok(None);
            }
            InstructionPhase::Pending
        }
        "sent" | "landed" => {
            let Some(command) = command else {
                return Ok(None);
            };
            // The saved command binds the observed effort to this exact session.
            // Never use its standing default to describe the completed instruction.
            let observed = crate::session_parameters::turn_effort(db, session, &command)?;
            if observed.as_deref() != serde_json::to_value(effort)?.as_str() {
                return Ok(None);
            }
            if state == "landed" {
                InstructionPhase::Finished
            } else if active_turn == Some(command.as_str()) {
                InstructionPhase::Active
            } else {
                return Ok(None);
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(Instruction {
        input_id: job.child_input.to_string(),
        effort,
        phase,
    }))
}
pub(crate) fn project(db: &Connection, snapshot: &mut Value) -> Result<()> {
    for value in snapshot["workers"].as_array_mut().into_iter().flatten() {
        let shown = match (value["trackId"].as_str(), value["sessionId"].as_str()) {
            (Some(track), Some(session)) => {
                instruction(db, track, session, value["activeTurn"].as_str())?
            }
            _ => None,
        };
        value["pairedInstruction"] = serde_json::to_value(shown)?;
    }
    Ok(())
}
