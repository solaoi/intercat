//! Original worker questions retained alongside choices made without an answer.
use crate::{
    Result,
    assumptions::Source,
    reports::{Question, Report},
};
use rusqlite::Transaction;
use serde_json::Value;
use uuid::Uuid;

pub fn record(
    tx: &Transaction<'_>,
    source: &Source,
    asked: &str,
    assumed: &str,
) -> Result<(String, bool)> {
    let clip: String = tx.query_row(
        "SELECT record FROM timeline_clips WHERE id=?1",
        [&source.clip],
        |row| row.get(0),
    )?;
    let clip: Value = serde_json::from_str(&clip)?;
    let (session, item): (String, String) = serde_json::from_str(&source.id)?;
    if clip["sessionId"] != session || clip["trackId"] != source.track {
        return Err("Question source does not match its clip.".into());
    }
    let turn: Uuid = clip["turnId"]
        .as_str()
        .ok_or("Question has no source turn.")?
        .parse()?;
    let report = Report::new(
        source.track.clone(),
        session.clone(),
        turn,
        item,
        source.text.clone(),
    );
    let id = Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        &serde_json::to_vec(&("assumed-question", &source.id, asked, assumed))?,
    )
    .to_string();
    let question = Question {
        id: id.clone(),
        report_id: report.id,
        track_id: source.track.clone(),
        session_id: session,
        turn_id: turn,
        revision: 1,
        kind: "question".into(),
        text: asked.into(),
        state: "assumed".into(),
        assumed: Some(assumed.into()),
        reply_utterance_id: None,
    };
    crate::report_store::put_question(tx, &question)?;
    // A pending or explicitly blocking question is independent of this choice.
    let closed = !clip["end"].is_null();
    Ok((id, closed))
}
