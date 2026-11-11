//! A saved Commit may suggest reviews; the proposal cannot send or create PRs.
use crate::{
    Result,
    decision::{Decision, Kind, Target},
    push_review::Selection,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;
pub(crate) fn candidates(db: &Connection) -> Result<Vec<Value>> {
    let Some(project) = crate::folders::project(db)? else {
        return Ok(vec![]);
    };
    let session: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key='session'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(session) = session else {
        return Ok(vec![]);
    };
    let session: Value = serde_json::from_str(&session)?;
    let Some(session_id) = session["id"].as_str().filter(|_| session["bound"] == true) else {
        return Ok(vec![]);
    };
    let mut through = BTreeMap::new();
    let mut created_at = 0;
    for folder in &project.folders {
        let saved:Option<(String,i64)>=db.query_row("SELECT json_extract(e.payload,'$.candidate.head'),e.ts FROM events e
            JOIN session_folders s ON s.session=e.session_id AND s.folder=json_extract(e.payload,'$.project.id')
            JOIN published_lineage p ON p.reference='refs/heads/intercat/'||s.slug AND p.head=json_extract(e.payload,'$.candidate.head')
            WHERE e.kind='integration.published' AND json_extract(e.payload,'$.status')='done'
            AND e.session_id=?1 AND json_extract(e.payload,'$.project.id')=?2 ORDER BY e.seq DESC LIMIT 1",params![session_id,folder.id],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        if let Some((sha, at)) = saved {
            sha.parse::<intercat_git::ObjectId>()?;
            through.insert(folder.id.clone(), sha);
            created_at = created_at.max(at);
        }
    }
    if through.is_empty() {
        return Ok(vec![]);
    }
    // A prepared or completed PR already supplies a place to resume that folder.
    let mut uncovered = BTreeMap::new();
    for (folder, sha) in through {
        let covered:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM pull_jobs j,json_each(j.record,'$.members') m WHERE j.session=?1 AND json_extract(m.value,'$.folderId')=?2 AND json_extract(m.value,'$.target.sha')=?3 AND json_extract(m.value,'$.phase')!='discarded')",params![session_id,folder,sha],|row|row.get(0))?;
        if !covered {
            uncovered.insert(folder, sha);
        }
    }
    let through = uncovered;
    if through.is_empty() {
        return Ok(vec![]);
    }
    let selection = Selection {
        folders: through.keys().cloned().collect(),
        through,
    };
    selection.validate()?;
    let source = json!({"sessionId":session_id,"selection":selection});
    let id = Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        &serde_json::to_vec(&("publication-review", &source))?,
    );
    if db.query_row(
        "SELECT EXISTS(SELECT 1 FROM proposal_answers WHERE proposal=?1)",
        [id.to_string()],
        |row| row.get::<_, bool>(0),
    )? {
        return Ok(vec![]);
    }
    let missing_base = selection.folders.iter().any(|id| {
        project
            .folders
            .iter()
            .find(|folder| folder.id == *id)
            .is_some_and(|folder| folder.base_branch.is_none())
    });
    Ok(vec![
        json!({"id":id,"kind":"publication","label":"Saved work","createdAt":created_at,"text":"Changes committed · Push and PR?","detail":"Opens the saved commits for destination review. Push and PR creation each need their own confirmation. You can leave this suggestion for later.","options":[{"action":"open-pr","text":"Review Push and PR","disabledReason":if missing_base{Some("no base branch yet")}else{None}},{"action":"later","text":"Not now"}],"source":source}),
    ])
}
pub(crate) fn respond(
    db: &rusqlite::Transaction<'_>,
    proposal: &Value,
    action: &crate::proposals::Action,
) -> Result<Value> {
    let mut result =
        json!({"proposalId":proposal["id"],"action":action,"source":proposal["source"]});
    match action {
        crate::proposals::Action::Later => {}
        crate::proposals::Action::OpenPr => {
            let selection: Selection =
                serde_json::from_value(proposal["source"]["selection"].clone())?;
            selection.validate()?;
            let input = Uuid::now_v7().to_string();
            crate::journal::insert_utterance(
                db,
                &input,
                "Review Push and PR for these committed changes.",
                &Target::None,
                None,
                &[],
                None,
                true,
                None,
            )?;
            let mut decision = Decision {
                kind: Kind::Command,
                track_id: None,
                boundary: None,
                command: Some("open-pr".into()),
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            };
            crate::app_commands::prepare_presentation(db, &mut decision)?;
            decision.scores["presentation"]["publication"] = serde_json::to_value(selection)?;
            db.execute("UPDATE utterances SET decision=?2,state=CASE WHEN state='held' THEN 'held' ELSE 'placed' END WHERE id=?1",params![input,serde_json::to_string(&decision)?])?;
            result["inputId"] = input.into();
        }
        _ => return Err("This proposal opens publication reviews only.".into()),
    }
    Ok(result)
}
