//! Explicit word waits and plan ordering use the same dependency graph.
//! A clip names its exact historic turn; it never aliases a later retry.
use crate::{Result, start_condition::Source};
use rusqlite::Connection;

fn input(id: &str) -> String {
    format!("input:{id}")
}
fn turn(session: &str, id: &str) -> String {
    format!("turn:{session}/{id}")
}

pub(crate) fn edges(db: &Connection) -> Result<Vec<(String, String)>> {
    let mut edges = Vec::new();
    for plan in crate::plan_store::list(db)? {
        for item in plan
            .items
            .iter()
            .filter(|item| item.state != crate::plans::ItemState::Cut)
        {
            if let Some(id) = item.utterance_id {
                edges.push((item.plan_item_id.to_string(), input(&id.to_string())));
            }
        }
    }
    let mut query = db.prepare("SELECT p.item,p.input FROM plan_inputs p JOIN utterances u ON u.id=p.input WHERE p.role='continuation' AND u.state NOT IN ('cut','landed')")?;
    for row in query.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (item, id) = row?;
        edges.push((item, input(&id)));
    }
    let mut query = db.prepare("SELECT id,json_extract(record,'$.sessionId'),json_extract(record,'$.turnId') FROM timeline_clips WHERE json_type(record,'$.sessionId')='text' AND json_type(record,'$.turnId')='text' AND json_extract(record,'$.repairRunId') IS NULL")?;
    for row in query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (id, session, source) = row?;
        edges.push((id, turn(&session, &source)));
    }
    let mut query = db.prepare("SELECT a.session,a.turn,u.id FROM utterances u JOIN operations o ON o.id=u.dispatch_command AND o.kind='turn.start' JOIN attachment_turns a ON a.command=o.id AND a.session=json_extract(o.payload,'$.sessionId') WHERE a.inherited=0 AND a.turn IS NOT NULL")?;
    let mut origins = std::collections::BTreeMap::new();
    for row in query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (session, id, origin) = row?;
        let key = turn(&session, &id);
        if origins
            .insert(key.clone(), origin.clone())
            .is_some_and(|prior| prior != origin)
        {
            return Err("The turn is linked to more than one request.".into());
        }
        edges.push((key, input(&origin)));
    }
    let mut query = db.prepare("SELECT id,decision FROM utterances WHERE state NOT IN ('cut','landed') AND json_extract(decision,'$.kind') IN ('fork','extend','schedule') AND json_extract(decision,'$.boundary') IN ('after-turn','after-clip')")?;
    for row in query.query_map([], |row| row.get::<_, String>(0))? {
        let id = row?;
        if let Some(source) = crate::start_condition::read(db, &id)?.source {
            let parent = match source {
                Source::Turn(source) => turn(&source.session_id, &source.turn_id.to_string()),
                Source::Clip { id, .. } => id,
                Source::PlanItem(source) => source.item_id.to_string(),
            };
            edges.push((input(&id), parent));
        }
    }
    Ok(edges)
}
pub(crate) fn validate(db: &Connection) -> Result<()> {
    let context = crate::plan_store::context(db, uuid::Uuid::nil())?;
    if crate::plans::cycle(&context.edges) {
        return Err(
            "This wait would create a dependency cycle. Choose different work or a start time."
                .into(),
        );
    }
    Ok(())
}
