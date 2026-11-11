//! Admission of the plan's individual instructions. The original request is
//! never a worker input. The journal checks the plan again at turn reservation.
use crate::{
    Result,
    plans::{Item, ItemState, Plan, State},
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::json;
use uuid::Uuid;

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS plan_inputs(input TEXT PRIMARY KEY REFERENCES utterances(id) ON DELETE CASCADE,plan TEXT NOT NULL REFERENCES plans(id) ON DELETE CASCADE,item TEXT NOT NULL,role TEXT NOT NULL CHECK(role IN ('initial','continuation')));
        CREATE INDEX IF NOT EXISTS plan_input_items ON plan_inputs(item,role);")?;
    Ok(())
}
pub(crate) fn owner(db: &Connection, input: &str) -> Result<Option<(Uuid, Uuid)>> {
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT plan,item FROM plan_inputs WHERE input=?1",
            [input],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(plan, item)| Ok((plan.parse()?, item.parse()?)))
        .transpose()
}
/// A track can run unrelated work between attempts of one plan item. Only
/// the input bound to this exact dispatched turn establishes ownership.
pub(crate) fn turn_owner(
    db: &Connection,
    session: &str,
    turn: &str,
) -> Result<Option<(Uuid, Uuid)>> {
    let values = db.prepare(
        "SELECT DISTINCT p.plan,p.item FROM plan_inputs p
         JOIN utterances u ON u.id=p.input
         JOIN operations o ON o.id=u.dispatch_command AND o.kind='turn.start'
         JOIN attachment_turns a ON a.command=o.id AND a.session=json_extract(o.payload,'$.sessionId')
         WHERE a.session=?1 AND a.turn=?2 AND a.inherited=0",
    )?.query_map(params![session,turn], |row| Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    match values.as_slice() {
        [] => Ok(None),
        [(plan, item)] => Ok(Some((plan.parse()?, item.parse()?))),
        _ => Err("The turn is linked to more than one plan item.".into()),
    }
}
pub(crate) fn assign_tracks(db: &Connection, plan: &mut Plan) -> Result<()> {
    let mut assigned = std::collections::BTreeMap::new();
    for item in plan
        .items
        .iter_mut()
        .filter(|item| item.state != ItemState::Cut)
    {
        if !item.track_ref.starts_with("new:") {
            continue;
        }
        let id = if let Some(id) = assigned.get(&item.track_ref) {
            *id
        } else {
            let id = Uuid::now_v7();
            let mut count: i64 = db.query_row(
                "SELECT count(*)+1 FROM tracks WHERE label NOT GLOB '*[0-9]*'",
                [],
                |row| row.get(0),
            )?;
            let mut label = String::new();
            while count > 0 {
                count -= 1;
                label.insert(0, (b'A' + (count % 26) as u8) as char);
                count /= 26;
            }
            db.execute(
                "INSERT INTO tracks VALUES(?1,?2,?3,'planned')",
                params![id.to_string(), label, item.title],
            )?;
            crate::folders::bind(db, &id.to_string(), &item.folder_id)?;
            assigned.insert(item.track_ref.clone(), id);
            id
        };
        item.track_ref = id.to_string();
    }
    crate::plan_history::assign_tracks(db, plan.id, &assigned)?;
    crate::plan_conflicts::assign_tracks(db, plan.id, &assigned)?;
    // A removed item on a surviving planned track restores onto that same
    // track. An entirely removed track need not be allocated until restored.
    for item in &mut plan.items {
        if let Some(id) = assigned.get(&item.track_ref) {
            item.track_ref = id.to_string();
        }
    }
    Ok(())
}
/// Existing plans keep their order on a shared real track. New-track names
/// are local to a plan and must never join unrelated plans' queues.
pub(crate) fn predecessors<'a>(plan: &'a Plan, item: &Item, plans: &'a [Plan]) -> Vec<&'a Item> {
    let mut result = Vec::new();
    if !item.track_ref.starts_with("new:") {
        for previous in plans.iter().take_while(|other| other.id != plan.id) {
            let mut items = previous
                .items
                .iter()
                .filter(|other| {
                    other.track_ref == item.track_ref
                        && other.state != ItemState::Cut
                        && !item.skip_order.contains(&other.plan_item_id)
                })
                .collect::<Vec<_>>();
            items.sort_by_key(|other| other.order);
            result.extend(items);
        }
    }
    let mut own = plan
        .items
        .iter()
        .filter(|other| {
            other.track_ref == item.track_ref
                && other.order < item.order
                && other.state != ItemState::Cut
                && !item.skip_order.contains(&other.plan_item_id)
        })
        .collect::<Vec<_>>();
    own.sort_by_key(|other| other.order);
    result.extend(own);
    result
}

pub(crate) fn waiting(
    db: &Connection,
    item: &Item,
    plan: &Plan,
    plans: &[Plan],
) -> Result<Option<String>> {
    for previous in predecessors(plan, item, plans) {
        let state = crate::plan_dependencies::read(db, plans, &previous.plan_item_id.to_string())?
            .map(|parent| parent.state)
            .unwrap_or_else(|| previous.state.clone());
        if state != ItemState::Succeeded
            || crate::plan_continuation::has_pending(db, &previous.plan_item_id.to_string())?
        {
            return Ok(Some(format!("after {}", previous.title)));
        }
    }
    for after in &item.after {
        let Some(parent) = crate::plan_dependencies::read(db, plans, &after.id)? else {
            return Ok(Some("The dependency is unavailable.".into()));
        };
        if crate::plan_continuation::has_pending(db, &after.id)? {
            return Ok(Some(format!("waiting for {}’s continuation", parent.title)));
        }
        let reason = match parent.state {
            ItemState::Succeeded => continue,
            ItemState::Cut => format!("held · {} was cut", parent.title),
            ItemState::Failed if parent.unclear => {
                format!("held · {}’s result unclear", parent.title)
            }
            ItemState::Failed => format!("held · {} didn’t finish", parent.title),
            _ => format!("after {}", parent.title),
        };
        return Ok(Some(reason));
    }
    Ok(None)
}
pub(crate) fn body(plan: &Plan, item: &Item) -> String {
    let mut text = format!(
        "Carry out only this plan item. The plan summary is context, not an instruction to carry out other items.\n\nPlan item: {}\n\nTask:\n{}\n\nConstraints:\n{}\n\nDone when:\n{}\n\nPlan summary:\n{}",
        item.title,
        item.instruction,
        item.constraints
            .iter()
            .map(|line| format!("- {line}"))
            .collect::<Vec<_>>()
            .join("\n"),
        item.done_when,
        plan.summary
    );
    if !plan.assumptions.is_empty() {
        text.push_str("\n\nPlanning assumptions (defaults only; the task above and any later correction take precedence):");
        for value in &plan.assumptions {
            text.push_str(&format!("\n- {}: {}", value.what, value.default));
        }
    }
    let cut = plan
        .items
        .iter()
        .filter(|other| other.state == ItemState::Cut)
        .map(|other| other.title.as_str())
        .collect::<Vec<_>>();
    if !cut.is_empty() {
        text.push_str(&format!(
            "\n\nNot to do (removed from this plan):\n{}",
            cut.join("\n")
        ));
    }
    text
}
fn prerequisites(
    db: &Connection,
    item: &Item,
    plan: &Plan,
    plans: &[Plan],
) -> Result<Option<String>> {
    if plan
        .locked_track
        .as_ref()
        .is_some_and(|track| track != &item.track_ref)
        || !crate::plan_store::track_available(db, &item.track_ref)?
    {
        return Ok(Some("The plan item's destination is unavailable.".into()));
    }
    waiting_for_dependencies(db, item, plan, plans)
}

pub(crate) fn waiting_for_dependencies(
    db: &Connection,
    item: &Item,
    plan: &Plan,
    plans: &[Plan],
) -> Result<Option<String>> {
    if let Some(reason) = waiting(db, item, plan, plans)? {
        return Ok(Some(reason));
    }
    let mut dependencies = predecessors(plan, item, plans)
        .into_iter()
        .map(|parent| (parent.plan_item_id.to_string(), false))
        .collect::<std::collections::BTreeMap<_, _>>();
    for after in &item.after {
        dependencies
            .entry(after.id.clone())
            .and_modify(|inherit| *inherit |= after.inherit)
            .or_insert(after.inherit);
    }
    for (id, inherit) in dependencies {
        let Some(parent) = crate::plan_dependencies::read(db, plans, &id)? else {
            return Ok(Some("The dependency is unavailable.".into()));
        };
        let needs_saved_files =
            (inherit || parent.folder != item.folder_id) && parent.track != item.track_ref;
        let Some(turn) = parent.turn else {
            if needs_saved_files {
                return Ok(Some(format!("waiting for {}’s saved files", parent.title)));
            }
            continue;
        };
        if turn.checkpoint.is_none() {
            return Ok(Some(format!("waiting for {}’s saved files", parent.title)));
        }
        let blocked:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM reports WHERE pending=1 AND json_extract(record,'$.sessionId')=?1 AND json_extract(record,'$.turnId')=?2) OR EXISTS(SELECT 1 FROM report_questions WHERE state IN ('open','queued','held') AND json_extract(record,'$.sessionId')=?1 AND json_extract(record,'$.turnId')=?2)",params![turn.session,turn.id.to_string()],|row|row.get(0))?;
        if blocked {
            return Ok(Some(format!("waiting for {}’s answer", parent.title)));
        }
    }
    Ok(None)
}
fn inherited_points(
    db: &Connection,
    item: &Item,
    plans: &[Plan],
) -> Result<Vec<crate::lineage::Point>> {
    let mut points = Vec::new();
    for after in item.after.iter().filter(|after| after.inherit) {
        let parent = crate::plan_dependencies::read(db, plans, &after.id)?
            .ok_or("The dependency is unavailable.")?;
        if parent.track == item.track_ref {
            continue;
        }
        if parent.folder != item.folder_id {
            return Err("Files cannot be inherited from another folder.".into());
        }
        let checkpoint = parent
            .turn
            .and_then(|turn| turn.checkpoint)
            .ok_or("The dependency has no saved checkpoint.")?;
        points.push(crate::lineage_store::point(db, &parent.track, &checkpoint)?);
    }
    Ok(points)
}
fn reference_sources(
    db: &Connection,
    item: &Item,
    plans: &[Plan],
) -> Result<Vec<crate::reference_volume::Source>> {
    let mut sources = Vec::new();
    for after in &item.after {
        let parent = crate::plan_dependencies::read(db, plans, &after.id)?
            .ok_or("The dependency is unavailable.")?;
        if parent.folder == item.folder_id || parent.track == item.track_ref {
            continue;
        }
        let checkpoint = parent
            .turn
            .and_then(|turn| turn.checkpoint)
            .ok_or("The reference has no saved checkpoint.")?;
        let point = crate::lineage_store::point(db, &parent.track, &checkpoint)?;
        let path: String = db.query_row(
            "SELECT json_extract(record,'$.path') FROM workspaces WHERE track_id=?1",
            [&parent.track],
            |row| row.get(0),
        )?;
        sources.push(crate::reference_volume::Source {
            track: point.track,
            checkpoint: point.sha,
            number: point.number,
            repository: path.into(),
        });
    }
    Ok(sources)
}
fn readiness(db: &Connection, item: &Item, plan: &Plan, plans: &[Plan]) -> Result<Option<String>> {
    if let Some(reason) = prerequisites(db, item, plan, plans)? {
        return Ok(Some(reason));
    }
    if !files_current(db, item, plans)? {
        return Ok(Some("Preparing updated dependency files".into()));
    }
    if let Some(input) = item.utterance_id
        && crate::plan_files::pending(db, &input.to_string())?.is_some()
    {
        return Ok(Some("Preparing changed dependencies".into()));
    }
    if !inherited_points(db, item, plans)?.is_empty() {
        let Some(input) = item.utterance_id else {
            return Ok(Some("Preparing inherited files".into()));
        };
        return crate::inheritance::waiting(db, &input.to_string());
    }
    Ok(None)
}
fn files_current(db: &Connection, item: &Item, plans: &[Plan]) -> Result<bool> {
    let Some(input) = item.utterance_id else {
        return Ok(false);
    };
    let inherited = crate::inheritance::dependencies(db, &input.to_string())?
        .map(|dependencies| dependencies.points)
        .unwrap_or_default();
    let references = crate::dependency_references::read(db, &input.to_string(), &item.track_ref)?
        .map(|recipe| recipe.sources)
        .unwrap_or_default();
    Ok(inherited == inherited_points(db, item, plans)?
        && references == reference_sources(db, item, plans)?)
}
fn enqueue(db: &Transaction<'_>, plan: &Plan, item: &mut Item) -> Result<()> {
    let plans = crate::plan_store::list(db)?;
    if item.utterance_id.is_some() && !files_current(db, item, &plans)? {
        crate::plan_edit::unstarted(db, item)?;
        if let Some(old) = item.utterance_id.take() {
            db.execute("UPDATE utterances SET state='cut',revision=revision+1,reason='Dependency result changed' WHERE id=?1",[old.to_string()])?;
        }
    }
    let Some(id) = item.utterance_id else {
        let id = Uuid::now_v7();
        let text = body(plan, item);
        crate::journal::insert_utterance(
            db,
            &id.to_string(),
            &text,
            &crate::decision::Target::Lock(item.track_ref.clone()),
            Some(&item.track_ref),
            &plan
                .attachments
                .iter()
                .map(|file| file.id)
                .collect::<Vec<_>>(),
            None,
            true,
            None,
        )?;
        db.execute(
            "INSERT INTO plan_inputs VALUES(?1,?2,?3,'initial')",
            params![
                id.to_string(),
                plan.id.to_string(),
                item.plan_item_id.to_string()
            ],
        )?;
        crate::inheritance::queue(
            db,
            &crate::inheritance::Dependencies {
                input: id.to_string(),
                track: item.track_ref.clone(),
                points: inherited_points(db, item, &plans)?,
            },
        )?;
        crate::dependency_references::queue(
            db,
            id,
            &item.track_ref,
            reference_sources(db, item, &plans)?,
        )?;
        let decision = json!({"kind":"schedule","trackId":item.track_ref,"boundary":"after-turn","command":null,"confidence":1.0,"inferenceMs":0.0,"timedOut":false,"scores":{}});
        db.execute(
            "UPDATE utterances SET state='placed',decision=?2,reason=NULL WHERE id=?1",
            params![id.to_string(), decision.to_string()],
        )?;
        crate::worker_hold::hold_blocked(db, &item.track_ref, &id.to_string())?;
        item.utterance_id = Some(id);
        return Ok(());
    };
    if crate::worker_hold::allowed(db, &item.track_ref, &id.to_string())? {
        db.execute("UPDATE utterances SET state='placed',reason=NULL WHERE id=?1 AND state='held' AND reason IN ('Turns stopped','Plan item restored','Held after this turn')",[id.to_string()])?;
    }
    Ok(())
}
pub(crate) fn admit(db: &Connection, input: &str) -> Result<()> {
    let Some((plan_id, item_id)) = owner(db, input)? else {
        return Ok(());
    };
    let mut plan = crate::plan_store::read(db, plan_id)?;
    let plans = crate::plan_store::list(db)?;
    let item = plan
        .items
        .iter()
        .find(|item| item.plan_item_id == item_id)
        .ok_or("The plan item is unavailable.")?;
    if crate::plan_continuation::is_continuation(db, input)? {
        if !crate::plan_continuation::input_allowed(db, input, &plan, item)? {
            return Err(crate::journal::StartSuperseded.into());
        }
        let item = plan
            .items
            .iter_mut()
            .find(|item| item.plan_item_id == item_id)
            .unwrap();
        item.track_ref = crate::continuation::current(db, &item.track_ref)?;
        item.utterance_id = Some(input.parse()?);
        item.state = ItemState::Running;
        item.detail = None;
        if plan.state == State::Done {
            plan.state = State::Running;
        }
        crate::fresh_words::route_plan(db, &mut plan)?;
        return crate::plan_store::save(db, &plan, "plan.item-continued");
    }
    if item.utterance_id.map(|id| id.to_string()).as_deref() != Some(input)
        || (plan.state != State::Running && !item.start_requested)
        || item.state != ItemState::Planned
        || readiness(db, item, &plan, &plans)?.is_some()
    {
        return Err(crate::journal::StartSuperseded.into());
    }
    let item = plan
        .items
        .iter_mut()
        .find(|item| item.plan_item_id == item_id)
        .unwrap();
    item.state = ItemState::Running;
    item.start_requested = false;
    crate::plan_store::save(db, &plan, "plan.item-started")
}
pub(crate) fn allowed(db: &Connection, input: &str) -> Result<bool> {
    let Some((plan_id, item_id)) = owner(db, input)? else {
        return Ok(true);
    };
    let plan = crate::plan_store::read(db, plan_id)?;
    let plans = crate::plan_store::list(db)?;
    let Some(item) = plan.items.iter().find(|item| item.plan_item_id == item_id) else {
        return Ok(false);
    };
    if crate::plan_continuation::is_continuation(db, input)? {
        return crate::plan_continuation::input_allowed(db, input, &plan, item);
    }
    Ok(
        item.utterance_id.map(|id| id.to_string()).as_deref() == Some(input)
            && (plan.state == State::Running || item.start_requested)
            && item.state == ItemState::Planned
            && readiness(db, item, &plan, &plans)?.is_none(),
    )
}
pub(crate) fn preparable(db: &Connection, input: &str) -> Result<bool> {
    let Some((plan_id, item_id)) = owner(db, input)? else {
        return Ok(true);
    };
    let plan = crate::plan_store::read(db, plan_id)?;
    let plans = crate::plan_store::list(db)?;
    let Some(item) = plan.items.iter().find(|item| item.plan_item_id == item_id) else {
        return Ok(false);
    };
    if crate::plan_continuation::is_continuation(db, input)? {
        return crate::plan_continuation::input_allowed(db, input, &plan, item);
    }
    Ok(
        item.utterance_id.map(|id| id.to_string()).as_deref() == Some(input)
            && (plan.state == State::Running
                || item.start_requested
                || crate::plan_files::pending(db, input)?.is_some())
            && item.state == ItemState::Planned
            && prerequisites(db, item, &plan, &plans)?.is_none()
            && files_current(db, item, &plans)?,
    )
}

/// Final message revisions can arrive after the checkpoint. Resolve the exact
/// saved turn again at admission, rather than trusting a prior scheduler pass.
pub(crate) fn settled_outcome(
    db: &Connection,
    origin: &crate::input_origin::Origin,
) -> Result<Option<(ItemState, Option<String>)>> {
    if !matches!(
        origin.phase,
        crate::input_origin::Phase::Finished | crate::input_origin::Phase::MovedOn
    ) || origin.checkpoint.is_none()
    {
        return Ok(None);
    }
    let result = crate::report_result::final_report(
        db,
        &origin.identity.session_id,
        &origin.identity.turn_id.to_string(),
    )?;
    let completed = crate::report_result::terminal_status(
        db,
        &origin.identity.session_id,
        &origin.identity.turn_id.to_string(),
    )?
    .as_deref()
        == Some("completed");
    let state = if completed
        && result
            .as_ref()
            .is_some_and(|result| result["kind"] == "done")
    {
        ItemState::Succeeded
    } else {
        ItemState::Failed
    };
    let detail = result
        .as_ref()
        .and_then(|result| result["reason"].as_str())
        .map(str::to_owned)
        .or_else(|| Some("Result unclear".into()));
    Ok(Some((state, detail)))
}

pub(crate) fn item_origin(
    db: &Connection,
    item: &Item,
) -> Result<Option<crate::input_origin::Origin>> {
    let Some(input) = item.utterance_id else {
        return Ok(None);
    };
    let retired: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM retired_inputs WHERE input=?1)",
        [input.to_string()],
        |row| row.get(0),
    )?;
    if retired {
        return Ok(None);
    }
    crate::input_origin::read(db, input)
}

pub(crate) fn advance(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    let all = crate::plan_store::list(&tx)?;
    for mut plan in all.clone() {
        let before = serde_json::to_string(&plan)?;
        for item in &mut plan.items {
            if !matches!(
                item.state,
                ItemState::Running | ItemState::Succeeded | ItemState::Failed
            ) {
                continue;
            }
            let Some(origin) = item_origin(&tx, item)? else {
                continue;
            };
            if let Some((state, detail)) = settled_outcome(&tx, &origin)? {
                item.state = state;
                item.detail = detail;
            }
        }
        crate::fresh_words::route_plan(&tx, &mut plan)?;
        if !crate::dispatch_control::paused(&tx)? {
            for index in 0..plan.items.len() {
                if plan.items[index].state != ItemState::Planned {
                    continue;
                }
                if plan.state != State::Running
                    && !plan.items[index].start_requested
                    && !crate::plan_files::has_origin(
                        &tx,
                        plan.items[index].plan_item_id,
                        &plan.items[index].track_ref,
                    )?
                {
                    continue;
                }
                let reason = prerequisites(&tx, &plan.items[index], &plan, &all)?;
                plan.items[index].detail = reason.clone();
                if reason.is_none() {
                    let mut item = plan.items[index].clone();
                    enqueue(&tx, &plan, &mut item)?;
                    item.detail = readiness(&tx, &item, &plan, &all)?;
                    plan.items[index] = item;
                }
            }
            if plan.state == State::Running
                && plan.items.iter().all(|item| {
                    matches!(
                        item.state,
                        ItemState::Succeeded | ItemState::Failed | ItemState::Cut
                    )
                })
            {
                plan.state = State::Done;
            }
        }
        if serde_json::to_string(&plan)? != before {
            crate::plan_store::save(&tx, &plan, "plan.progress")?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    mod parallel_acceptance {
        include!("plan_parallel_acceptance.rs");
    }
    use super::*;
    use crate::{
        journal::Journal,
        plan_store::{Action, Command, Generated},
        plans::{Draft, DraftItem},
        request_sources::Submission,
        secrets::Key,
    };
    async fn fixture(inherit: bool) -> Result<(tempfile::TempDir, Journal, Plan)> {
        fixture_layout(inherit, None, false).await
    }
    use crate::test_host::IdleTestHost;
    struct QueuedFixture {
        root: tempfile::TempDir,
        journal: Journal,
        plan: Plan,
        folder: String,
        child_folder: String,
    }
    async fn queued_fixture(
        cross_folder_marker: Option<&str>,
        automatic: bool,
        request: &str,
    ) -> Result<QueuedFixture> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("project");
        std::fs::create_dir(&source)?;
        assert!(
            std::process::Command::new("/usr/bin/git")
                .args(["init", "-q", "-b", "main"])
                .arg(&source)
                .status()?
                .success()
        );
        let mut paths = vec![source.clone()];
        if cross_folder_marker.is_some() {
            let second = root.path().join("second-project");
            std::fs::create_dir(&second)?;
            assert!(
                std::process::Command::new("/usr/bin/git")
                    .args(["init", "-q", "-b", "main"])
                    .arg(&second)
                    .status()?
                    .success()
            );
            for (path, name) in [(&source, "a-base.txt"), (&second, "b-base.txt")] {
                std::fs::write(path.join(name), "Dummy original base\n")?;
                assert!(
                    std::process::Command::new("/usr/bin/git")
                        .current_dir(path)
                        .args(["add", name])
                        .status()?
                        .success()
                );
                assert!(
                    std::process::Command::new("/usr/bin/git")
                        .current_dir(path)
                        .args([
                            "-c",
                            "user.name=Dummy",
                            "-c",
                            "user.email=dummy@localhost",
                            "-c",
                            "core.hooksPath=/dev/null",
                            "commit",
                            "-qm",
                            "Dummy base"
                        ])
                        .status()?
                        .success()
                );
            }
            paths.push(second);
        }
        let project =
            crate::project::inspect(Uuid::now_v7(), "Dummy project".into(), paths).await?;
        let folder = project.folders[0].id();
        let child_folder = if cross_folder_marker.is_some() {
            project.folders[1].id()
        } else {
            folder.clone()
        };
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        journal.configure_project(Uuid::now_v7(), project).await?;
        if automatic {
            journal
                .change_plans_start(crate::plan_start::Change {
                    op_id: Uuid::now_v7(),
                    revision: 0,
                    mode: crate::plan_start::Mode::AfterDelay,
                })
                .await?;
        }

        journal
            .stage_document(Submission {
                op_id: Uuid::now_v7(),
                text: request.into(),
                target: crate::decision::Target::New,
                selected_track_id: None,
                attachment_ids: vec![],
                sources: None,
                execute_allowed: automatic,
                plan_item: None,
            })
            .await?;
        if automatic {
            let input = journal.claim_input().await?.unwrap();
            journal
                .apply_input(
                    input.id,
                    input.revision,
                    crate::decision::Outcome::Classified(crate::decision::Decision {
                        kind: crate::decision::Kind::Plan,
                        track_id: None,
                        boundary: None,
                        command: None,
                        confidence: 1.,
                        inference_ms: 0.,
                        timed_out: false,
                        scores: json!({}),
                    }),
                )
                .await?;
        }
        let plan = journal.plans().await?.pop().unwrap();
        Ok(QueuedFixture {
            root,
            journal,
            plan,
            folder,
            child_folder,
        })
    }
    async fn fixture_layout(
        inherit: bool,
        cross_folder_marker: Option<&str>,
        automatic: bool,
    ) -> Result<(tempfile::TempDir, Journal, Plan)> {
        fixture_layout_assumptions(inherit, cross_folder_marker, automatic, vec![]).await
    }
    async fn fixture_layout_assumptions(
        inherit: bool,
        cross_folder_marker: Option<&str>,
        automatic: bool,
        assumptions: Vec<crate::plans::DraftAssumption>,
    ) -> Result<(tempfile::TempDir, Journal, Plan)> {
        let QueuedFixture {
            root,
            journal,
            plan,
            folder,
            child_folder,
        } = queued_fixture(
            cross_folder_marker,
            automatic,
            "Original request sentinel; never repeat to workers.",
        )
        .await?;
        let item = |id: &str, track: &str, after: Vec<crate::plans::Dependency>| DraftItem {
            id: id.into(),
            track_ref: track.into(),
            folder_id: if id == "two" {
                child_folder.clone()
            } else {
                folder.clone()
            },
            title: format!("Job {id}"),
            instruction: if let Some(marker) = cross_folder_marker {
                if id == "one" {
                    format!(
                        "Create one.txt containing exactly {marker} followed by a newline. Do not edit other files, make commits, run commands, use the network, or ask questions."
                    )
                } else if id == "two" {
                    "Read one.txt from the read-only dependency snapshot supplied with this request, and copy its entire content to two.txt in your primary workspace. Do not create one.txt in your primary workspace or modify the reference. If it is unavailable, report failure. Do not make commits, run commands, use the network, or ask questions.".into()
                } else {
                    "Do not run this removed job.".into()
                }
            } else {
                format!(
                    "{}Create {id}.txt containing exactly DUMMY_{id}. Do not edit other files.",
                    if inherit && id == "two" {
                        "First read one.txt and verify it contains DUMMY_one. If the file is missing or different, report failure without creating two.txt. "
                    } else {
                        ""
                    }
                )
            },
            constraints: vec!["Use dummy data.".into()],
            done_when: format!("Dummy {id} exists."),
            after,
        };
        let draft = Draft {
            assumptions,
            conflicts: vec![],
            summary: "Three dummy jobs".into(),
            items: vec![
                item("one", "new:1", vec![]),
                item(
                    "two",
                    "new:2",
                    vec![crate::plans::Dependency {
                        id: "one".into(),
                        inherit,
                    }],
                ),
                item("three", "new:1", vec![]),
            ],
        };
        journal
            .begin_plan(plan.id, plan.generation.id, plan.rev)
            .await?;
        journal
            .generated_plan(
                plan.id,
                plan.generation.id,
                plan.rev,
                Generated::Draft(draft),
                vec![],
            )
            .await?;
        let plan = journal.plans().await?.pop().unwrap();
        Ok((root, journal, plan))
    }
    fn command(plan: &Plan, action: Action) -> Command {
        Command {
            op_id: Uuid::now_v7(),
            plan_id: plan.id,
            revision: plan.rev,
            action,
        }
    }
    async fn ready_editor(root: &std::path::Path) -> Result<crate::editor::Manager> {
        let source = root.join("editor/intercat_editor");
        std::fs::create_dir_all(&source)?;
        std::fs::write(source.join("__init__.py"), "")?;
        std::fs::write(
            source.join("server.py"),
            r#"import json
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def do_GET(self):
  body=b'{"accepting":true,"modelLoaded":true}';self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_POST(self):
  self.rfile.read(int(self.headers['Content-Length']));body=b'{"tokens":10,"cachePrefix":false,"stages":{"question":{"selected":"0"}},"inferenceMs":1}';self.send_response(200);self.send_header('X-Intercat-Inference','started');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
print(json.dumps({'host':'127.0.0.1','port':server.server_port}),flush=True)
server.serve_forever()
"#,
        )?;
        let model = root.join("dummy-model");
        std::fs::create_dir(&model)?;
        let editor = crate::editor::Manager::new(&root.join("editor"), &root.join("editor.log"))?;
        editor
            .configure(crate::editor::Config {
                python: "/usr/bin/python3".into(),
                model,
            })
            .await?;
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !editor.status().model_loaded {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await?;
        Ok(editor)
    }
    #[tokio::test]
    async fn only_reviewed_start_queues_individual_jobs_and_hold_blocks_admission() -> Result<()> {
        let (root, journal, plan) = fixture(false).await?;
        journal.advance_plans().await?;
        assert_eq!(
            journal.snapshot().await?["tracks"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert!(
            journal
                .plan_command(command(&plan, Action::Start))
                .await
                .is_err()
        );
        let cut = journal
            .plan_command(command(
                &plan,
                Action::Cut {
                    item_id: plan.items[2].plan_item_id,
                },
            ))
            .await?;
        journal.plan_command(command(&cut, Action::Review)).await?;
        let request = command(&cut, Action::Start);
        let started = journal.plan_command(request.clone()).await?;
        assert_eq!(journal.plan_command(request).await?.rev, started.rev);
        assert_eq!(started.state, State::Running);
        assert_eq!(started.items[2].track_ref, started.items[0].track_ref);
        journal.advance_plans().await?;
        let snapshot = journal.snapshot().await?;
        assert_eq!(snapshot["tracks"].as_array().unwrap().len(), 2);
        let inputs = snapshot["utterances"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|word| word["planItemId"].is_string())
            .collect::<Vec<_>>();
        assert_eq!(inputs.len(), 1);
        let text = inputs[0]["text"].as_str().unwrap();
        assert!(text.contains("Create one.txt containing exactly DUMMY_one."));
        assert!(text.contains("Not to do (removed from this plan):\nJob three"));
        assert!(!text.contains("Create three.txt") && !text.contains("Original request sentinel"));
        assert_eq!(inputs[0]["planRunnable"], true);
        let current = journal.plans().await?.pop().unwrap();
        journal
            .plan_command(command(&current, Action::Hold))
            .await?;
        let db = Connection::open(root.path().join("db"))?;
        assert!(!allowed(&db, inputs[0]["id"].as_str().unwrap())?);
        assert!(admit(&db, inputs[0]["id"].as_str().unwrap()).is_err());
        Ok(())
    }

    struct StartedParent {
        root: tempfile::TempDir,
        journal: Journal,
        plan: Plan,
        track: String,
        path: std::path::PathBuf,
        sha: gix::ObjectId,
        turn: Uuid,
    }
    async fn started_parent(inherit: bool) -> Result<StartedParent> {
        started_parent_layout(inherit, None).await
    }
    async fn started_parent_layout(
        inherit: bool,
        cross_folder: Option<&str>,
    ) -> Result<StartedParent> {
        started_parent_assumptions(inherit, cross_folder, vec![], false).await
    }
    async fn started_parent_assumptions(
        inherit: bool,
        cross_folder: Option<&str>,
        assumptions: Vec<crate::plans::DraftAssumption>,
        keep_first: bool,
    ) -> Result<StartedParent> {
        let (root, journal, mut plan) =
            fixture_layout_assumptions(inherit, cross_folder, false, assumptions).await?;
        if keep_first {
            plan = journal
                .plan_command(command(
                    &plan,
                    Action::KeepAssumption {
                        assumption_id: plan.assumptions[0].id,
                    },
                ))
                .await?;
        }
        let plan = journal
            .plan_command(command(
                &plan,
                Action::Cut {
                    item_id: plan.items[2].plan_item_id,
                },
            ))
            .await?;
        journal.plan_command(command(&plan, Action::Review)).await?;
        journal.plan_command(command(&plan, Action::Start)).await?;
        journal.advance_plans().await?;
        let plan = journal.plans().await?.pop().unwrap();
        let input = plan.items[0].utterance_id.unwrap();
        let track = plan.items[0].track_ref.clone();
        let path = root.path().join("parent");
        std::fs::create_dir(&path)?;
        gix::init(&path)?;
        let repo = intercat_git::Repository::open(&path)?;
        let base = repo.empty_starting_point()?;
        let sha = repo.checkpoint_commit(
            repo.write_tree(&[("reference.txt", repo.write_blob(b"saved parent\n")?)])?,
            base,
            "Dummy reference",
        )?;
        journal
            .workspace_result(crate::preparation::Workspace {
                denied_roots: Vec::new(),
                track_id: track.clone(),
                path: path.clone(),
                reference: "refs/intercat/test/tracks/A".into(),
                base: base.to_string(),
                state: "ready".into(),
                error: None,
                attachment_roots: vec![],
            })
            .await?;
        let project = journal.snapshot().await?["projectSecrets"]["projectId"]
            .as_str()
            .unwrap()
            .to_owned();
        journal
            .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
            .await?;
        let key = Key::new([7; 32]);
        let catalog = journal
            .prepare_project_secrets(crate::project_secrets::Prepare {
                op_id: Uuid::now_v7(),
                project_id: project.clone(),
                revision: 0,
                fingerprints: vec![],
                key_check: key
                    .check()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            })
            .await?;
        journal.confirm_fingerprints(catalog.current).await?;
        journal
            .activate_project_secrets(crate::project_secrets::Load {
                project_id: project,
                revision: 0,
                entries: vec![],
            })
            .await?;
        let worker = crate::runtime::WorkerState {
            track_id: track.clone(),
            phase: "ready".into(),
            session_id: Some("dummy-parent-session".into()),
            secret_revision: Some(0),
            identity: None,
            detail: None,
            active_turn: None,
            approvals: vec![],
            output: None,
            checkpoint_pending: false,
            checkpoint: None,
            prompts: vec![],
        };
        journal.worker_state(worker).await?;
        let start = Uuid::now_v7();
        let turn = Uuid::now_v7();
        let revision = journal.read_utterance(input.to_string()).await?["revision"]
            .as_i64()
            .unwrap();
        journal
            .reserve_turn(
                start,
                input.to_string(),
                revision,
                track.clone(),
                json!({"sessionId":"dummy-parent-session","commandId":start}),
                None,
            )
            .await?;
        journal.try_msp(track.clone(),"dummy-parent-session".into(),crate::clock::Stamp::now(),json!({"method":"item/updated","params":{"sessionId":"dummy-parent-session","item":{"kind":"userMessage","itemId":"dummy-input","commandId":start,"turnId":turn,"text":"Dummy parent","revision":1,"status":"completed"}}}),None,None)?.await??;
        Ok(StartedParent {
            root,
            journal,
            plan,
            track,
            path,
            sha,
            turn,
        })
    }

    async fn finish_parent(scene: &StartedParent, turn: Uuid, result: &str) -> Result<()> {
        finish_parent_tree(scene, turn, result, None).await
    }

    async fn finish_parent_tree(
        scene: &StartedParent,
        turn: Uuid,
        result: &str,
        tree: Option<gix::ObjectId>,
    ) -> Result<()> {
        let journal = &scene.journal;
        journal.try_msp(scene.track.clone(), "dummy-parent-session".into(), crate::clock::Stamp::now(),
            json!({"method":"item/completed","params":{"sessionId":"dummy-parent-session","item":{
                "itemId":format!("final-{turn}"),"turnId":turn,"kind":"agentMessage","messagePhase":"finalAnswer","status":"completed","text":result,"revision":1
            }}}),None,None)?.await??;
        journal.try_msp(scene.track.clone(), "dummy-parent-session".into(), crate::clock::Stamp::now(),
            json!({"method":"turn/completed","params":{"sessionId":"dummy-parent-session","turnId":turn,"terminal":"completed"}}),None,None)?.await??;
        let mut worker: crate::runtime::WorkerState = serde_json::from_value(
            journal.snapshot().await?["workers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|worker| worker["trackId"] == scene.track)
                .unwrap()
                .clone(),
        )?;
        let parent = worker
            .checkpoint
            .as_ref()
            .map(|point| point.sha.parse())
            .transpose()?
            .unwrap_or(scene.sha);
        let sha = {
            let repo = intercat_git::Repository::open(&scene.path)?;
            repo.checkpoint_commit(
                tree.unwrap_or(repo.tree_id(parent)?),
                parent,
                &format!("Dummy result {turn}"),
            )?
        };
        let checkpoint = crate::checkpoint::Checkpoint {
            turn_id: turn,
            sha: sha.to_string(),
            reference: format!("refs/intercat/test/checkpoints/{turn}"),
            unmerged_paths: vec![],
            live_processes: false,
        };
        worker.phase = "ready".into();
        worker.active_turn = None;
        worker.checkpoint_pending = false;
        worker.checkpoint = Some(checkpoint.clone());
        journal.worker_state(worker).await?;
        let id = Uuid::new_v5(&turn, b"checkpoint").to_string();
        journal
            .intent(
                &id,
                "checkpoint.save",
                json!({"trackId":scene.track,"checkpoint":checkpoint}),
            )
            .await?;
        journal.finish(&id, "dispatched").await?;
        journal.finish(&id, "complete").await?;
        journal.advance_plans().await
    }

    async fn later_plan_on_track(journal: &Journal, folder: &str, track: &str) -> Result<Plan> {
        journal
            .stage_document(Submission {
                op_id: Uuid::now_v7(),
                text: "A later dummy plan on the existing track".into(),
                target: crate::decision::Target::New,
                selected_track_id: None,
                attachment_ids: vec![],
                sources: None,
                execute_allowed: false,
                plan_item: None,
            })
            .await?;
        let plan = journal.plans().await?.pop().unwrap();
        journal
            .begin_plan(plan.id, plan.generation.id, plan.rev)
            .await?;
        journal
            .generated_plan(
                plan.id,
                plan.generation.id,
                plan.rev,
                Generated::Draft(Draft {
                    assumptions: vec![],
                    conflicts: vec![],
                    summary: "Later dummy work".into(),
                    items: vec![DraftItem {
                        id: "later".into(),
                        track_ref: track.into(),
                        folder_id: folder.into(),
                        title: "Later work on A".into(),
                        instruction: "Use dummy data".into(),
                        constraints: vec![],
                        done_when: "Dummy work done".into(),
                        after: vec![],
                    }],
                }),
                vec![],
            )
            .await?;
        let later = journal.plans().await?.pop().unwrap();
        journal
            .plan_command(command(&later, Action::Review))
            .await?;
        journal.plan_command(command(&later, Action::Start)).await
    }

    #[tokio::test]
    async fn a_later_plan_waits_for_an_earlier_plan_on_the_same_track() -> Result<()> {
        let (root, journal, initial) = fixture(false).await?;
        let cut = journal
            .plan_command(command(
                &initial,
                Action::Cut {
                    item_id: initial.items[2].plan_item_id,
                },
            ))
            .await?;
        journal.plan_command(command(&cut, Action::Review)).await?;
        let mut first = journal.plan_command(command(&cut, Action::Start)).await?;
        first.items[0].state = ItemState::Failed;
        let db = Connection::open(root.path().join("db"))?;
        crate::clock::register(&db)?;
        crate::plan_store::save(&db, &first, "fixture.failed-earlier-plan")?;
        let later = later_plan_on_track(
            &journal,
            &first.items[0].folder_id,
            &first.items[0].track_ref,
        )
        .await?;
        journal.advance_plans().await?;
        assert!(
            crate::plan_store::read(&db, later.id)?.items[0]
                .utterance_id
                .is_none()
        );
        assert!(waiting(&db, &later.items[0], &later, &journal.plans().await?)?.is_some());
        first.items[0].state = ItemState::Succeeded;
        crate::plan_store::save(&db, &first, "fixture.earlier-plan-completed")?;
        journal.advance_plans().await?;
        assert!(
            crate::plan_store::read(&db, later.id)?.items[0]
                .utterance_id
                .is_some()
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_failed_predecessor_in_another_plan_can_be_skipped_explicitly() -> Result<()> {
        let (root, journal, initial) = fixture(false).await?;
        let cut = journal
            .plan_command(command(
                &initial,
                Action::Cut {
                    item_id: initial.items[2].plan_item_id,
                },
            ))
            .await?;
        journal.plan_command(command(&cut, Action::Review)).await?;
        let mut first = journal.plan_command(command(&cut, Action::Start)).await?;
        first.items[0].state = ItemState::Failed;
        let db = Connection::open(root.path().join("db"))?;
        crate::clock::register(&db)?;
        crate::plan_store::save(&db, &first, "fixture.failed-earlier-plan")?;
        let later = later_plan_on_track(
            &journal,
            &first.items[0].folder_id,
            &first.items[0].track_ref,
        )
        .await?;
        let held = journal.plan_command(command(&later, Action::Hold)).await?;
        let changed = journal
            .plan_command(command(
                &held,
                Action::StartWithout {
                    item_id: held.items[0].plan_item_id,
                    dependency_id: first.items[0].plan_item_id.to_string(),
                },
            ))
            .await?;
        assert_eq!(changed.state, State::Held);
        assert!(
            changed.items[0]
                .skip_order
                .contains(&first.items[0].plan_item_id)
        );
        assert!(changed.items[0].start_requested);
        journal.advance_plans().await?;
        assert!(
            crate::plan_store::read(&db, later.id)?.items[0]
                .utterance_id
                .is_some()
        );
        Ok(())
    }

    #[tokio::test]
    async fn moving_an_older_plan_onto_a_later_plans_track_keeps_validation_and_execution_order_equal()
    -> Result<()> {
        let (root, journal, initial) = fixture(false).await?;
        let mut first = initial;
        for index in [1, 2] {
            first = journal
                .plan_command(command(
                    &first,
                    Action::Cut {
                        item_id: first.items[index].plan_item_id,
                    },
                ))
                .await?;
        }
        journal
            .plan_command(command(&first, Action::Review))
            .await?;
        let first = journal.plan_command(command(&first, Action::Start)).await?;
        let db = Connection::open(root.path().join("db"))?;
        crate::clock::register(&db)?;
        let target = Uuid::now_v7().to_string();
        db.execute(
            "INSERT INTO tracks VALUES(?1,'B','Dummy target','planned')",
            [&target],
        )?;
        crate::folders::bind(&db, &target, &first.items[0].folder_id)?;
        let mut later = later_plan_on_track(&journal, &first.items[0].folder_id, &target).await?;
        later.items[0].after = vec![crate::plans::Dependency {
            id: first.items[0].plan_item_id.to_string(),
            inherit: false,
        }];
        crate::plan_store::save(&db, &later, "fixture.later-depends-on-first")?;
        let moved = journal
            .plan_command(command(
                &first,
                Action::MoveTo {
                    item_id: first.items[0].plan_item_id,
                    track_id: target,
                },
            ))
            .await?;
        assert!(waiting(&db, &moved.items[0], &moved, &journal.plans().await?)?.is_none());
        assert!(waiting(&db, &later.items[0], &later, &journal.plans().await?)?.is_some());
        journal.advance_plans().await?;
        assert!(
            crate::plan_store::read(&db, moved.id)?.items[0]
                .utterance_id
                .is_some()
        );
        assert!(
            crate::plan_store::read(&db, later.id)?.items[0]
                .utterance_id
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn new_track_names_are_not_ordered_across_separate_plans() -> Result<()> {
        let (_root, journal, first) = fixture(false).await?;
        let mut later = first.clone();
        later.id = Uuid::now_v7();
        later.items.truncate(1);
        later.items[0].plan_item_id = Uuid::now_v7();
        let all = vec![first, later.clone()];
        assert!(predecessors(&later, &later.items[0], &all).is_empty());
        let db = Connection::open(_root.path().join("db"))?;
        assert!(waiting(&db, &later.items[0], &later, &all)?.is_none());
        drop(journal);
        Ok(())
    }

    #[tokio::test]
    async fn auto_start_waits_for_the_successful_parents_unanswered_question() -> Result<()> {
        let scene = started_parent(false).await?;
        finish_parent(
            &scene,
            scene.turn,
            "Result: done — dummy prerequisite is ready",
        )
        .await?;
        let mut db = Connection::open(scene.root.path().join("db"))?;
        crate::clock::register(&db)?;
        crate::plan_start::install(&db)?;
        let mut plan = scene.journal.plans().await?.pop().unwrap();
        crate::plan_edit::invalidate_input(&db, &mut plan.items[1])?;
        plan.state = State::Draft;
        plan.start_mode = crate::plan_start::Mode::AfterDelay;
        plan.execute_allowed = true;
        plan.next_revision()?;
        crate::plan_store::save(&db, &plan, "fixture.awaiting-auto-start")?;
        let question = crate::reports::Question {
            id: Uuid::now_v7().to_string(),
            report_id: "dummy-late-report".into(),
            track_id: scene.track.clone(),
            session_id: "dummy-parent-session".into(),
            turn_id: scene.turn,
            revision: 1,
            kind: "plain".into(),
            text: "Dummy question".into(),
            state: "open".into(),
            assumed: None,
            reply_utterance_id: None,
        };
        db.execute(
            "INSERT INTO report_questions VALUES(?1,?2,?3,'open',?4)",
            params![
                question.id,
                question.report_id,
                scene.track,
                serde_json::to_string(&question)?
            ],
        )?;
        let owner = Uuid::now_v7();
        for tick in 0..=24 {
            let update = crate::plan_start::observe(
                &mut db,
                crate::plan_start::Observation {
                    owner,
                    sequence: tick + 1,
                    idle: true,
                    plans: vec![crate::plan_start::Seen {
                        plan_id: plan.id,
                        revision: plan.rev,
                    }],
                },
                &crate::clock::Stamp {
                    epoch_ms: tick as i64 * 250,
                    mono_ms: tick as i64 * 250,
                    boot_id: "test-auto-start".into(),
                    timeline_ms: tick as i64 * 250,
                },
            )?;
            assert!(update.countdowns.is_empty());
            assert!(update.started.is_empty());
        }
        db.execute("UPDATE report_questions SET state='answered',record=json_set(record,'$.state','answered') WHERE id=?1",[&question.id])?;
        let update = crate::plan_start::observe(
            &mut db,
            crate::plan_start::Observation {
                owner,
                sequence: 26,
                idle: true,
                plans: vec![crate::plan_start::Seen {
                    plan_id: plan.id,
                    revision: plan.rev,
                }],
            },
            &crate::clock::Stamp {
                epoch_ms: 6_250,
                mono_ms: 6_250,
                boot_id: "test-auto-start".into(),
                timeline_ms: 6_250,
            },
        )?;
        assert_eq!(update.countdowns.len(), 1);
        assert_eq!(update.countdowns[0].remaining_ms, 5_000);
        assert!(update.started.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn late_result_revisions_gate_explicit_and_same_track_dependents_before_advance()
    -> Result<()> {
        for implicit in [false, true] {
            let scene = started_parent(false).await?;
            let db = Connection::open(scene.root.path().join("db"))?;
            crate::clock::register(&db)?;
            if implicit {
                let mut plan = scene.journal.plans().await?.pop().unwrap();
                plan.items[1].track_ref = scene.track.clone();
                plan.items[1].after.clear();
                crate::plan_store::save(&db, &plan, "fixture.implicit-order")?;
            }
            finish_parent(&scene, scene.turn, "Result: done — first saved result").await?;
            scene.journal.advance_plans().await?;
            let plan = scene.journal.plans().await?.pop().unwrap();
            let child = plan.items[1].utterance_id.unwrap().to_string();
            assert!(allowed(&db, &child)?);
            for (revision, text, released) in [
                (2, "Result: partial — a late correction", false),
                (1, "Result: done — an obsolete message", false),
                (3, "A late message without its result footer.", false),
                (4, "Result: done — the corrected saved result", true),
            ] {
                scene.journal.try_msp(scene.track.clone(), "dummy-parent-session".into(), crate::clock::Stamp::now(),
                    json!({"method":"item/updated","params":{"sessionId":"dummy-parent-session","item":{
                        "itemId":format!("final-{}",scene.turn),"turnId":scene.turn,"kind":"agentMessage","messagePhase":"finalAnswer","status":"completed","text":text,"revision":revision
                    }}}),None,None)?.await??;
                assert_eq!(
                    allowed(&db, &child)?,
                    released,
                    "implicit={implicit}, revision={revision}"
                );
                assert_eq!(preparable(&db, &child)?, released);
                scene.journal.advance_plans().await?;
                let current = scene.journal.plans().await?.pop().unwrap();
                assert_eq!(
                    current.items[0].state,
                    if released {
                        ItemState::Succeeded
                    } else {
                        ItemState::Failed
                    }
                );
                if revision == 3 {
                    assert_eq!(current.items[0].detail.as_deref(), Some("Result unclear"));
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn updated_parent_replaces_cross_folder_reference_before_child_start() -> Result<()> {
        let scene = started_parent_layout(false, Some("DUMMY_REFERENCE")).await?;
        finish_parent(&scene, scene.turn, "Result: done — original reference").await?;
        scene.journal.advance_plans().await?;
        let before = scene.journal.plans().await?.pop().unwrap();
        let child = &before.items[1];
        let old_input = child.utterance_id.unwrap();
        let db = Connection::open(scene.root.path().join("db"))?;
        crate::clock::register(&db)?;
        let old =
            crate::dependency_references::read(&db, &old_input.to_string(), &child.track_ref)?
                .unwrap();
        let work = scene.root.path().join("dummy-child-workspace");
        std::fs::create_dir(&work)?;
        std::fs::write(work.join("own.txt"), "KEEP_CHILD_FILE\n")?;
        let params = || json!({"commandId":Uuid::now_v7(),"sessionId":"dummy-child","input":[{"type":"skill","source":"user","selector":"intercat-report","arguments":"Read the dummy reference."}]});
        let result = async {
            let prepared = scene
                .journal
                .prepare_turn(
                    old_input.to_string(),
                    child.track_ref.clone(),
                    "dummy-child".into(),
                    work.clone(),
                    params(),
                )
                .await?;
            let old_root =
                std::path::PathBuf::from(prepared.params()["workspaceRoots"][1].as_str().unwrap());
            assert_eq!(
                std::fs::read_to_string(
                    old_root.join(old.sources[0].name()?).join("reference.txt")
                )?,
                "saved parent\n"
            );
            drop(prepared);
            let continuation = scene
                .journal
                .stage_document(Submission {
                    op_id: Uuid::now_v7(),
                    text: "Update the dummy reference.".into(),
                    target: crate::decision::Target::Lock(scene.track.clone()),
                    selected_track_id: Some(scene.track.clone()),
                    attachment_ids: vec![],
                    sources: None,
                    execute_allowed: true,
                    plan_item: Some(crate::request_sources::PlanItemContext {
                        plan_id: before.id,
                        item_id: before.items[0].plan_item_id,
                        revision: before.rev,
                    }),
                })
                .await?;
            assert!(!allowed(&db, &old_input.to_string())?);
            let turn = start_continuation(&scene, continuation["id"].as_str().unwrap()).await?;
            let updated_tree = {
                let repo = intercat_git::Repository::open(&scene.path)?;
                repo.write_tree(&[(
                    "reference.txt",
                    repo.write_blob(b"UPDATED_DUMMY_REFERENCE\n")?,
                )])?
            };
            finish_parent_tree(
                &scene,
                turn,
                "Result: done — updated reference",
                Some(updated_tree),
            )
            .await?;
            assert!(!allowed(&db, &old_input.to_string())?);
            scene.journal.advance_plans().await?;
            let after = scene.journal.plans().await?.pop().unwrap();
            let input = after.items[1].utterance_id.unwrap();
            assert_ne!(input, old_input);
            let latest =
                crate::dependency_references::read(&db, &input.to_string(), &child.track_ref)?
                    .unwrap();
            assert_eq!(latest.sources[0].number, old.sources[0].number + 1);
            assert_ne!(latest.sources[0].checkpoint, old.sources[0].checkpoint);
            assert!(
                scene
                    .journal
                    .input_inheritance(input.to_string())
                    .await?
                    .is_none()
            );
            let prepared = scene
                .journal
                .prepare_turn(
                    input.to_string(),
                    child.track_ref.clone(),
                    "dummy-child".into(),
                    work.clone(),
                    params(),
                )
                .await?;
            let latest_root =
                std::path::PathBuf::from(prepared.params()["workspaceRoots"][1].as_str().unwrap());
            assert_ne!(old_root, latest_root);
            assert_eq!(
                std::fs::read_to_string(
                    latest_root
                        .join(latest.sources[0].name()?)
                        .join("reference.txt")
                )?,
                "UPDATED_DUMMY_REFERENCE\n"
            );
            assert_eq!(
                std::fs::read_to_string(work.join("own.txt"))?,
                "KEEP_CHILD_FILE\n"
            );
            assert!(!work.join("reference.txt").exists());
            assert!(
                std::fs::write(
                    latest_root
                        .join(latest.sources[0].name()?)
                        .join("reference.txt"),
                    "BAD"
                )
                .is_err()
            );
            assert!(allowed(&db, &input.to_string())?);
            drop(prepared);
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }
        .await;
        scene
            .journal
            .unmount_dependency_references(&child.track_ref)
            .await?;
        result
    }

    async fn start_continuation(scene: &StartedParent, input: &str) -> Result<Uuid> {
        let start = Uuid::now_v7();
        let turn = Uuid::now_v7();
        let revision = scene.journal.read_utterance(input.into()).await?["revision"]
            .as_i64()
            .unwrap();
        scene
            .journal
            .reserve_turn(
                start,
                input.into(),
                revision,
                scene.track.clone(),
                json!({"sessionId":"dummy-parent-session","commandId":start}),
                None,
            )
            .await?;
        scene.journal.try_msp(scene.track.clone(),"dummy-parent-session".into(),crate::clock::Stamp::now(),
            json!({"method":"item/updated","params":{"sessionId":"dummy-parent-session","item":{
                "itemId":format!("input-{turn}"),"commandId":start,"turnId":turn,"kind":"userMessage","text":"Dummy continuation","status":"completed","revision":1
            }}}),None,None)?.await??;
        Ok(turn)
    }

    async fn ordinary_input(scene: &StartedParent, text: &str) -> Result<String> {
        let id = Uuid::now_v7();
        scene
            .journal
            .stage_utterance(
                id,
                text.into(),
                crate::decision::Target::Lock(scene.track.clone()),
                None,
                vec![],
            )
            .await?;
        let input = scene
            .journal
            .claim_input()
            .await?
            .ok_or("Missing ordinary dummy request")?;
        assert_eq!(input.id, id.to_string());
        scene
            .journal
            .apply_input(
                input.id.clone(),
                input.revision,
                crate::decision::Outcome::Classified(crate::decision::Decision {
                    kind: crate::decision::Kind::Extend,
                    track_id: Some(scene.track.clone()),
                    boundary: Some("after-turn".into()),
                    command: None,
                    confidence: 1.0,
                    inference_ms: 0.0,
                    timed_out: false,
                    scores: json!({}),
                }),
            )
            .await?;
        Ok(input.id)
    }

    async fn external_failed_dependency() -> Result<(StartedParent, String, String)> {
        let scene = started_parent(true).await?;
        finish_parent(
            &scene,
            scene.turn,
            "Result: done — original plan item finished",
        )
        .await?;
        let plan = scene.journal.plans().await?.pop().unwrap();
        scene
            .journal
            .plan_command(command(&plan, Action::Hold))
            .await?;
        let input = ordinary_input(
            &scene,
            "Complete this separate dummy report, retaining reference.txt.",
        )
        .await?;
        let turn = start_continuation(&scene, &input).await?;
        finish_parent(
            &scene,
            turn,
            "Result: partial — the separate dummy report needs another step",
        )
        .await?;
        let db = Connection::open(scene.root.path().join("db"))?;
        assert!(
            crate::plan_execution::turn_owner(&db, "dummy-parent-session", &turn.to_string())?
                .is_none()
        );
        let clip: String = db.query_row("SELECT json_extract(record,'$.current') FROM timeline_turns WHERE session='dummy-parent-session' AND turn=?1", [turn.to_string()], |r| r.get(0))?;
        let plan = scene.journal.plans().await?.pop().unwrap();
        let owner = Uuid::now_v7();
        let token = Uuid::now_v7();
        let editing = scene
            .journal
            .edit_plan(crate::plan_rewrite::Request {
                owner,
                action: crate::plan_rewrite::Action::Begin {
                    plan_id: plan.id,
                    item_id: plan.items[1].plan_item_id,
                    token,
                    revision: plan.rev,
                },
            })
            .await?
            .unwrap();
        scene
            .journal
            .edit_plan(crate::plan_rewrite::Request {
                owner,
                action: crate::plan_rewrite::Action::Dependencies {
                    plan_id: editing.id,
                    token,
                    revision: editing.rev,
                    op_id: Uuid::now_v7(),
                    after: vec![crate::plans::Dependency {
                        id: clip.clone(),
                        inherit: true,
                    }],
                },
            })
            .await?;
        Ok((scene, clip, input))
    }

    #[tokio::test]
    async fn external_retry_is_atomic_and_only_its_explicit_turn_releases_the_dependency()
    -> Result<()> {
        let (scene, clip, original) = external_failed_dependency().await?;
        let db = Connection::open(scene.root.path().join("db"))?;
        let plan = scene.journal.plans().await?.pop().unwrap();
        assert!(crate::dependency_retry::retryable(
            &db,
            std::slice::from_ref(&plan),
            &clip
        )?);
        let request = command(
            &plan,
            Action::RetryDependency {
                item_id: plan.items[1].plan_item_id,
                dependency_id: clip.clone(),
                clip_id: clip.clone(),
            },
        );
        let retry_input = Uuid::new_v5(&request.op_id, b"retry-dependency").to_string();
        db.execute_batch("CREATE TRIGGER fail_external_retry BEFORE INSERT ON operations WHEN NEW.kind='plan.command' BEGIN SELECT RAISE(ABORT,'dummy save failure'); END;")?;
        assert!(scene.journal.plan_command(request.clone()).await.is_err());
        assert_eq!(scene.journal.plans().await?[0].rev, plan.rev);
        assert_eq!(
            db.query_row("SELECT count(*) FROM dependency_retries", [], |r| r
                .get::<_, i64>(0))?,
            0
        );
        assert!(
            scene
                .journal
                .read_utterance(retry_input.clone())
                .await
                .is_err()
        );
        db.execute_batch("DROP TRIGGER fail_external_retry;")?;
        let queued = scene.journal.plan_command(request.clone()).await?;
        assert_eq!(queued.state, State::Held);
        assert_eq!(scene.journal.plan_command(request).await?.rev, queued.rev);
        assert!(!crate::dependency_retry::retryable(
            &db,
            std::slice::from_ref(&queued),
            &clip
        )?);
        let attempt = crate::dependency_retry::for_input(&db, &retry_input)?.unwrap();
        assert_eq!(attempt.task_input.to_string(), original);
        assert!(crate::dependency_retry::allowed(&db, &retry_input)?);
        assert!(
            crate::dependency_retry::instructions(&db, &retry_input)?
                .unwrap()
                .contains("Complete this separate dummy report")
        );
        let unrelated = ordinary_input(&scene, "Finish an unrelated dummy task.").await?;
        let other_turn = start_continuation(&scene, &unrelated).await?;
        finish_parent(
            &scene,
            other_turn,
            "Result: done — unrelated work is finished",
        )
        .await?;
        assert_eq!(
            crate::plan_dependencies::read(&db, &scene.journal.plans().await?, &clip)?
                .unwrap()
                .state,
            ItemState::Planned
        );
        let retry_turn = start_continuation(&scene, &retry_input).await?;
        finish_parent(
            &scene,
            retry_turn,
            "Result: done — the separate dummy report is finished",
        )
        .await?;
        let held = scene.journal.plans().await?.pop().unwrap();
        assert_eq!(held.state, State::Held);
        assert!(held.items[1].utterance_id.is_none());
        let parent =
            crate::plan_dependencies::read(&db, std::slice::from_ref(&held), &clip)?.unwrap();
        assert_eq!(parent.state, ItemState::Succeeded);
        assert_eq!(parent.turn.as_ref().unwrap().id, retry_turn);
        let latest = crate::dependency_retry::current_clip(
            &db,
            &crate::input_origin::read(&db, retry_input.parse()?)?.unwrap(),
        )?
        .unwrap();
        assert_ne!(latest, clip);
        let projection = crate::plan_dependencies::projection(&db, std::slice::from_ref(&held))?;
        assert_eq!(projection[&clip].clip_id.as_deref(), Some(latest.as_str()));
        scene
            .journal
            .plan_command(command(&held, Action::Review))
            .await?;
        scene
            .journal
            .plan_command(command(&held, Action::Start))
            .await?;
        scene.journal.advance_plans().await?;
        let plan = scene.journal.plans().await?.pop().unwrap();
        let recipe = crate::inheritance::dependencies(
            &db,
            &plan.items[1].utterance_id.unwrap().to_string(),
        )?
        .unwrap();
        assert_eq!(
            recipe.points[0].sha,
            parent.turn.unwrap().checkpoint.unwrap()
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM dependency_retries", [], |r| r
                .get::<_, i64>(0))?,
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn retrying_a_saved_plan_clip_keeps_the_item_binding_without_changing_the_old_clip()
    -> Result<()> {
        let scene = started_parent(false).await?;
        finish_parent(
            &scene,
            scene.turn,
            "Result: partial — the dummy item needs another step",
        )
        .await?;
        let db = Connection::open(scene.root.path().join("db"))?;
        let plan = scene.journal.plans().await?.pop().unwrap();
        let held = scene
            .journal
            .plan_command(command(&plan, Action::Hold))
            .await?;
        let clip:String=db.query_row("SELECT json_extract(record,'$.current') FROM timeline_turns WHERE session='dummy-parent-session' AND turn=?1",[scene.turn.to_string()],|r|r.get(0))?;
        let owner = Uuid::now_v7();
        let token = Uuid::now_v7();
        let edit = scene
            .journal
            .edit_plan(crate::plan_rewrite::Request {
                owner,
                action: crate::plan_rewrite::Action::Begin {
                    plan_id: held.id,
                    item_id: held.items[1].plan_item_id,
                    token,
                    revision: held.rev,
                },
            })
            .await?
            .unwrap();
        let plan = scene
            .journal
            .edit_plan(crate::plan_rewrite::Request {
                owner,
                action: crate::plan_rewrite::Action::Dependencies {
                    plan_id: edit.id,
                    token,
                    revision: edit.rev,
                    op_id: Uuid::now_v7(),
                    after: vec![crate::plans::Dependency {
                        id: clip.clone(),
                        inherit: false,
                    }],
                },
            })
            .await?
            .unwrap();
        let request = command(
            &plan,
            Action::RetryDependency {
                item_id: plan.items[1].plan_item_id,
                dependency_id: clip.clone(),
                clip_id: clip.clone(),
            },
        );
        let input = Uuid::new_v5(&request.op_id, b"retry-dependency").to_string();
        let queued = scene.journal.plan_command(request).await?;
        assert_eq!(queued.rev, plan.rev + 1);
        assert_eq!(
            crate::plan_execution::owner(&db, &input)?,
            Some((plan.id, plan.items[0].plan_item_id))
        );
        assert!(scene.journal.snapshot().await?["editHistory"]["undo"]["blocked"].is_null());
        let turn = start_continuation(&scene, &input).await?;
        finish_parent(
            &scene,
            turn,
            "Result: done — the retried dummy item is complete",
        )
        .await?;
        let plan = scene.journal.plans().await?.pop().unwrap();
        assert_eq!(plan.items[0].state, ItemState::Succeeded);
        assert_eq!(
            crate::plan_dependencies::read(&db, std::slice::from_ref(&plan), &clip)?
                .unwrap()
                .turn
                .unwrap()
                .id,
            turn
        );
        let old_turn: String = db.query_row(
            "SELECT json_extract(record,'$.turnId') FROM timeline_clips WHERE id=?1",
            [clip],
            |r| r.get(0),
        )?;
        assert_eq!(old_turn, scene.turn.to_string());
        Ok(())
    }

    #[tokio::test]
    async fn external_retry_rejects_a_changed_source_and_an_older_restored_attempt() -> Result<()> {
        let (mut scene, clip, original) = external_failed_dependency().await?;
        let db = Connection::open(scene.root.path().join("db"))?;
        let plan = scene.journal.plans().await?.pop().unwrap();
        let bad = command(
            &plan,
            Action::RetryDependency {
                item_id: plan.items[1].plan_item_id,
                dependency_id: clip.clone(),
                clip_id: Uuid::now_v7().to_string(),
            },
        );
        assert!(scene.journal.plan_command(bad).await.is_err());
        let request = command(
            &plan,
            Action::RetryDependency {
                item_id: plan.items[1].plan_item_id,
                dependency_id: clip.clone(),
                clip_id: clip.clone(),
            },
        );
        let first = Uuid::new_v5(&request.op_id, b"retry-dependency").to_string();
        scene.journal.plan_command(request).await?;
        let source = crate::input_origin::read(&db, original.parse()?)?.unwrap();
        for (revision, text, allowed) in [
            (2, "A report without its required result line.", true),
            (3, "Result: done — late correction", false),
            (4, "Result: partial — correction needs work", true),
        ] {
            scene.journal.try_msp(scene.track.clone(),"dummy-parent-session".into(),crate::clock::Stamp::now(),json!({"method":"item/completed","params":{"sessionId":"dummy-parent-session","item":{
                "itemId":format!("final-{}",source.identity.turn_id),"turnId":source.identity.turn_id,"kind":"agentMessage","messagePhase":"finalAnswer","status":"completed","text":text,"revision":revision
            }}}),None,None)?.await??;
            assert_eq!(crate::dependency_retry::allowed(&db, &first)?, allowed);
            if !allowed {
                assert!(start_continuation(&scene, &first).await.is_err());
            }
        }
        scene
            .journal
            .cut_input(Uuid::now_v7(), first.clone())
            .await?;
        let plan = scene.journal.plans().await?.pop().unwrap();
        let request = command(
            &plan,
            Action::RetryDependency {
                item_id: plan.items[1].plan_item_id,
                dependency_id: clip.clone(),
                clip_id: clip.clone(),
            },
        );
        let second = Uuid::new_v5(&request.op_id, b"retry-dependency").to_string();
        scene.journal.plan_command(request).await?;
        let old = scene.journal.read_utterance(first.clone()).await?;
        scene
            .journal
            .edit_history(crate::timeline_history::Request {
                op_id: Uuid::now_v7(),
                action: crate::timeline_history::Action::Restore {
                    input: first.clone(),
                    revision: old["revision"].as_i64().unwrap(),
                },
            })
            .await?;
        assert!(!crate::dependency_retry::allowed(&db, &first)?);
        assert!(crate::dependency_retry::allowed(&db, &second)?);
        assert!(
            crate::input_start::unavailable(
                &db,
                &first,
                scene.journal.read_utterance(first.clone()).await?["revision"]
                    .as_i64()
                    .unwrap(),
                crate::input_start::Permission::Button
            )?
            .is_some()
        );
        assert!(start_continuation(&scene, &first).await.is_err());
        scene.journal = Journal::open(scene.root.path().join("db"), Key::new([7; 32])).await?;
        let old = scene.journal.read_utterance(first).await?;
        assert_eq!(old["dependencyRetryRunnable"], false);
        assert!(old["reason"].as_str().unwrap().contains("newer retry"));
        assert_eq!(
            crate::dependency_retry::latest(&db, &clip)?
                .unwrap()
                .input
                .to_string(),
            second
        );
        assert!(
            crate::dependency_retry::instructions(&db, &second)?
                .unwrap()
                .contains("Complete this separate dummy report")
        );
        db.execute(
            "INSERT INTO retired_inputs VALUES(?1,'dummy-parent-session')",
            [original],
        )?;
        assert!(crate::dependency_retry::latest(&db, &clip)?.is_none());
        assert!(!crate::dependency_retry::allowed(&db, &second)?);
        assert!(
            !crate::plan_dependencies::projection(&db, &crate::plan_store::list(&db)?)?[&clip]
                .retryable
        );
        Ok(())
    }

    #[tokio::test]
    async fn a_missing_dependency_keeps_change_and_start_without_available() -> Result<()> {
        let (scene, clip, _) = external_failed_dependency().await?;
        let db = Connection::open(scene.root.path().join("db"))?;
        db.execute("DELETE FROM timeline_clips WHERE id=?1", [&clip])?;
        let plan = scene.journal.plans().await?.pop().unwrap();
        let locations = crate::plan_dependencies::projection(&db, std::slice::from_ref(&plan))?;
        assert!(locations[&clip].unavailable);
        assert!(!locations[&clip].retryable);
        let updated = scene
            .journal
            .plan_command(command(
                &plan,
                Action::StartWithout {
                    item_id: plan.items[1].plan_item_id,
                    dependency_id: clip,
                },
            ))
            .await?;
        assert_eq!(updated.state, State::Held);
        scene.journal.advance_plans().await?;
        let plan = scene.journal.plans().await?.pop().unwrap();
        assert!(plan.items[1].after.is_empty());
        assert!(plan.items[1].start_requested);
        assert!(allowed(
            &db,
            &plan.items[1].utterance_id.unwrap().to_string()
        )?);
        Ok(())
    }

    #[tokio::test]
    async fn partial_continue_rebinds_only_its_item_and_uses_the_new_saved_result() -> Result<()> {
        let scene = started_parent(false).await?;
        scene
            .journal
            .stage_utterance(
                Uuid::now_v7(),
                "Fix a dummy README typo, a separate task.".into(),
                crate::decision::Target::Lock(scene.track.clone()),
                None,
                vec![],
            )
            .await?;
        let unrelated = scene.journal.claim_input().await?.unwrap();
        scene
            .journal
            .apply_input(
                unrelated.id.clone(),
                unrelated.revision,
                crate::decision::Outcome::Classified(crate::decision::Decision {
                    kind: crate::decision::Kind::Extend,
                    track_id: Some(scene.track.clone()),
                    boundary: Some("after-turn".into()),
                    command: None,
                    confidence: 1.0,
                    inference_ms: 0.0,
                    timed_out: false,
                    scores: json!({}),
                }),
            )
            .await?;
        finish_parent(&scene, scene.turn, "Result: partial — dummy work remains").await?;
        let first = scene.journal.plans().await?.pop().unwrap();
        assert_eq!(first.items[0].state, ItemState::Failed);
        assert!(first.items[1].utterance_id.is_none());
        scene
            .journal
            .plan_command(command(&first, Action::Hold))
            .await?;
        let proposal = scene.journal.snapshot().await?["proposal"].clone();
        assert_eq!(proposal["source"]["turnId"], scene.turn.to_string());
        let reply_command = crate::proposals::Command {
            op_id: Uuid::now_v7(),
            proposal_id: proposal["id"].as_str().unwrap().parse()?,
            action: crate::proposals::Action::Continue,
        };
        let before = scene.journal.plans().await?.pop().unwrap().rev;
        let fault = Connection::open(scene.root.path().join("db"))?;
        fault.execute_batch("CREATE TRIGGER fail_dummy_continuation BEFORE INSERT ON proposal_answers BEGIN SELECT RAISE(ABORT,'dummy save failure'); END;")?;
        assert!(
            scene
                .journal
                .answer_proposal(reply_command.clone())
                .await
                .is_err()
        );
        assert_eq!(scene.journal.plans().await?.pop().unwrap().rev, before);
        assert_eq!(
            fault.query_row(
                "SELECT count(*) FROM plan_inputs WHERE role='continuation'",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            0
        );
        fault.execute_batch("DROP TRIGGER fail_dummy_continuation;")?;
        let answer = scene.journal.answer_proposal(reply_command.clone()).await?;
        assert_eq!(scene.journal.answer_proposal(reply_command).await?, answer);
        let input = answer["inputId"].as_str().unwrap();
        let word = scene.journal.read_utterance(input.into()).await?;
        assert_eq!(word["planItemId"], first.items[0].plan_item_id.to_string());
        assert_eq!(word["planContinuation"]["title"], first.items[0].title);
        assert_eq!(word["decision"]["kind"], "extend");
        let saved_decision: String = fault.query_row(
            "SELECT decision FROM utterances WHERE id=?1",
            [input],
            |row| row.get(0),
        )?;
        fault.execute("UPDATE utterances SET decision=json_set(decision,'$.trackId','different-worker') WHERE id=?1",[input])?;
        assert!(!allowed(&fault, input)?);
        assert!(admit(&fault, input).is_err());
        fault.execute(
            "UPDATE utterances SET decision=?2 WHERE id=?1",
            params![input, saved_decision],
        )?;
        assert!(allowed(&fault, input)?);
        assert!(
            crate::plan_continuation::instructions(&fault, input)?
                .unwrap()
                .contains(&first.items[0].done_when)
        );
        assert!(
            !word["text"]
                .as_str()
                .unwrap()
                .contains("Original request sentinel")
        );
        let unrelated_turn = start_continuation(&scene, &unrelated.id.to_string()).await?;
        finish_parent(
            &scene,
            unrelated_turn,
            "Result: done — the unrelated dummy typo is fixed",
        )
        .await?;
        let still_failed = scene.journal.plans().await?.pop().unwrap();
        assert_eq!(still_failed.items[0].state, ItemState::Failed);
        assert!(still_failed.items[1].utterance_id.is_none());
        let unrelated_clips = scene
            .journal
            .timeline_window(crate::timeline::Window {
                tracks: vec![scene.track.clone()],
                start: 0,
                end: i64::MAX,
                since: None,
            })
            .await?;
        let other = unrelated_clips["clips"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|clip| clip["turnId"] == unrelated_turn.to_string())
            .collect::<Vec<_>>();
        assert!(!other.is_empty());
        assert!(other.iter().all(|clip| clip.get("planItemId").is_none()));
        let turn = start_continuation(&scene, input).await?;
        let running = scene.journal.plans().await?.pop().unwrap();
        assert_eq!(running.items[0].utterance_id.unwrap().to_string(), input);
        assert_eq!(running.items[0].state, ItemState::Running);
        assert!(running.items[1].utterance_id.is_none());
        finish_parent(&scene, turn, "Result: done — dummy work now complete").await?;
        // The next scheduler pass observes the newly saved parent outcome.
        scene.journal.advance_plans().await?;
        let complete = scene.journal.plans().await?.pop().unwrap();
        assert_eq!(complete.items[0].state, ItemState::Succeeded);
        assert_eq!(complete.state, State::Held);
        assert!(complete.items[1].utterance_id.is_none());
        scene
            .journal
            .plan_command(command(&complete, Action::Review))
            .await?;
        scene
            .journal
            .plan_command(command(&complete, Action::Start))
            .await?;
        scene.journal.advance_plans().await?;
        assert!(
            scene.journal.plans().await?[0].items[1]
                .utterance_id
                .is_some()
        );
        let db = Connection::open(scene.root.path().join("db"))?;
        assert_eq!(
            turn_owner(&db, "dummy-parent-session", &scene.turn.to_string())?,
            Some((complete.id, complete.items[0].plan_item_id))
        );
        assert_eq!(
            turn_owner(&db, "dummy-parent-session", &turn.to_string())?,
            Some((complete.id, complete.items[0].plan_item_id))
        );
        assert!(turn_owner(&db, "other-session", &turn.to_string())?.is_none());
        assert!(turn_owner(&db, "dummy-parent-session", &Uuid::now_v7().to_string())?.is_none());
        let clips = scene
            .journal
            .timeline_window(crate::timeline::Window {
                tracks: vec![scene.track.clone()],
                start: 0,
                end: i64::MAX,
                since: None,
            })
            .await?;
        for expected in [scene.turn, turn] {
            assert!(
                clips["clips"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|clip| clip["turnId"] == expected.to_string()
                        && clip["planItemId"] == complete.items[0].plan_item_id.to_string())
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_report_answer_keeps_its_item_and_waits_for_the_continuation_checkpoint() -> Result<()>
    {
        let scene = started_parent(false).await?;
        scene
            .journal
            .observe_report(crate::reports::Report::new(
                scene.track.clone(),
                "dummy-parent-session".into(),
                scene.turn,
                "dummy-question".into(),
                "Waiting for you: Choose a dummy format.".into(),
            ))
            .await?;
        finish_parent(
            &scene,
            scene.turn,
            "Result: done — waiting for a format answer",
        )
        .await?;
        let questions = scene.journal.snapshot().await?["reportQuestions"].clone();
        let question = &questions[0];
        assert_eq!(
            question["planItemId"],
            scene.plan.items[0].plan_item_id.to_string()
        );
        let id = Uuid::now_v7();
        scene
            .journal
            .respond_report(
                id,
                question["id"].as_str().unwrap().into(),
                question["revision"].as_i64().unwrap(),
                crate::reports::Answer::Reply {
                    text: "Use the dummy plain-text format.".into(),
                },
            )
            .await?;
        let input = id.to_string();
        assert_eq!(
            scene.journal.read_utterance(input.clone()).await?["planItemId"],
            scene.plan.items[0].plan_item_id.to_string()
        );
        scene.journal.advance_plans().await?;
        assert!(
            scene.journal.plans().await?[0].items[1]
                .utterance_id
                .is_none()
        );
        let turn = start_continuation(&scene, &input).await?;
        finish_parent(&scene, turn, "Result: partial — one dummy detail remains").await?;
        let final_plan = scene.journal.plans().await?.pop().unwrap();
        assert_eq!(final_plan.items[0].state, ItemState::Failed);
        assert!(final_plan.items[1].utterance_id.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn item_context_is_atomic_revision_bound_and_included_only_in_worker_instructions()
    -> Result<()> {
        let scene = started_parent(false).await?;
        finish_parent(&scene, scene.turn, "Result: partial — dummy detail remains").await?;
        let plan = scene.journal.plans().await?.pop().unwrap();
        let request = Submission {
            op_id: Uuid::now_v7(),
            text: "Change the dummy output to plain text.".into(),
            target: crate::decision::Target::Lock(scene.track.clone()),
            selected_track_id: None,
            attachment_ids: vec![],
            sources: None,
            execute_allowed: true,
            plan_item: Some(crate::request_sources::PlanItemContext {
                plan_id: plan.id,
                item_id: plan.items[0].plan_item_id,
                revision: plan.rev,
            }),
        };
        let mut stale = request.clone();
        stale.op_id = Uuid::now_v7();
        stale.plan_item.as_mut().unwrap().revision += 1;
        assert!(scene.journal.stage_document(stale).await.is_err());
        let mut wrong = request.clone();
        wrong.op_id = Uuid::now_v7();
        wrong.target = crate::decision::Target::Lock(plan.items[1].track_ref.clone());
        assert!(scene.journal.stage_document(wrong).await.is_err());
        let mut unstarted = request.clone();
        unstarted.op_id = Uuid::now_v7();
        unstarted.plan_item.as_mut().unwrap().item_id = plan.items[1].plan_item_id;
        unstarted.target = crate::decision::Target::Lock(plan.items[1].track_ref.clone());
        assert!(scene.journal.stage_document(unstarted).await.is_err());
        let mut plan_only = request.clone();
        plan_only.op_id = Uuid::now_v7();
        plan_only.execute_allowed = false;
        assert!(scene.journal.stage_document(plan_only).await.is_err());
        let db = Connection::open(scene.root.path().join("db"))?;
        db.execute_batch("CREATE TRIGGER fail_context BEFORE INSERT ON plan_inputs WHEN NEW.role='continuation' BEGIN SELECT RAISE(ABORT,'dummy save failure'); END;")?;
        assert!(scene.journal.stage_document(request.clone()).await.is_err());
        assert!(
            scene
                .journal
                .read_utterance(request.op_id.to_string())
                .await
                .is_err()
        );
        assert_eq!(scene.journal.plans().await?.pop().unwrap().rev, plan.rev);
        db.execute_batch("DROP TRIGGER fail_context;")?;
        let word = scene.journal.stage_document(request.clone()).await?;
        assert_eq!(word["text"], request.text);
        assert_eq!(word["state"], "placed");
        assert_eq!(word["decision"]["kind"], "extend");
        assert_eq!(word["planItemId"], plan.items[0].plan_item_id.to_string());
        assert_eq!(scene.journal.stage_document(request.clone()).await?, word);
        let reopened = Journal::open(scene.root.path().join("db"), Key::new([7; 32])).await?;
        assert_eq!(reopened.stage_document(request.clone()).await?, word);
        let mut changed = request.clone();
        changed.plan_item.as_mut().unwrap().item_id = plan.items[1].plan_item_id;
        assert!(scene.journal.stage_document(changed).await.is_err());
        let params = json!({"commandId":Uuid::now_v7(),"sessionId":"dummy-parent-session","input":[{"type":"skill","source":"user","selector":"intercat-report","arguments":request.text}]});
        let prepared = scene
            .journal
            .prepare_turn(
                request.op_id.to_string(),
                scene.track.clone(),
                "dummy-parent-session".into(),
                scene.path.clone(),
                params,
            )
            .await?;
        let body = prepared.params()["input"][0]["arguments"].as_str().unwrap();
        assert!(body.contains(&plan.items[0].done_when));
        assert!(body.contains("Change the dummy output to plain text."));
        assert!(!body.contains("Original request sentinel"));
        Ok(())
    }

    #[tokio::test]
    async fn start_without_removes_only_the_named_dependency_and_keeps_the_rest_held() -> Result<()>
    {
        for implicit in [false, true] {
            let scene = started_parent(false).await?;
            finish_parent(&scene, scene.turn, "Result: failed — dummy parent failed").await?;
            let mut plan = scene.journal.plans().await?.pop().unwrap();
            let db = Connection::open(scene.root.path().join("db"))?;
            crate::clock::register(&db)?;
            if implicit {
                plan.items[1].track_ref = plan.items[0].track_ref.clone();
                plan.items[1].after.clear();
                crate::plan_store::save(&db, &plan, "fixture.implicit-wait")?;
            }
            plan = scene
                .journal
                .plan_command(command(
                    &plan,
                    Action::Restore {
                        item_id: plan.items[2].plan_item_id,
                    },
                ))
                .await?;
            let request = command(
                &plan,
                Action::StartWithout {
                    item_id: plan.items[1].plan_item_id,
                    dependency_id: plan.items[0].plan_item_id.to_string(),
                },
            );
            db.execute_batch("CREATE TRIGGER fail_start_without BEFORE INSERT ON operations WHEN NEW.kind='plan.command' BEGIN SELECT RAISE(ABORT,'dummy save failure'); END;")?;
            assert!(scene.journal.plan_command(request.clone()).await.is_err());
            let unchanged = scene.journal.plans().await?.pop().unwrap();
            assert_eq!(unchanged.rev, plan.rev);
            assert!(!unchanged.items[1].start_requested);
            assert_eq!(unchanged.items[1].after, plan.items[1].after);
            db.execute_batch("DROP TRIGGER fail_start_without;")?;
            let changed = scene.journal.plan_command(request.clone()).await?;
            assert_eq!(
                scene.journal.plan_command(request.clone()).await?.rev,
                changed.rev
            );
            assert_eq!(changed.state, State::Held);
            assert!(changed.items[1].start_requested);
            assert!(changed.items[1].after.is_empty());
            assert_eq!(
                changed.items[1]
                    .skip_order
                    .contains(&plan.items[0].plan_item_id),
                implicit
            );
            scene.journal.advance_plans().await?;
            let queued = scene.journal.plans().await?.pop().unwrap();
            let input = queued.items[1].utterance_id.unwrap().to_string();
            assert!(queued.items[2].utterance_id.is_none());
            assert_eq!(
                scene.journal.read_utterance(input.clone()).await?["planManualStart"],
                true
            );
            assert!(allowed(&db, &input)?);
            let history = scene.journal.snapshot().await?["editHistory"].clone();
            assert_eq!(history["undo"]["label"], "Start without dependency");
            scene
                .journal
                .edit_history(crate::timeline_history::Request {
                    op_id: Uuid::now_v7(),
                    action: crate::timeline_history::Action::Undo {
                        entry: history["undo"]["entryId"].as_str().unwrap().into(),
                    },
                })
                .await?;
            let restored = scene.journal.plans().await?.pop().unwrap();
            assert!(!restored.items[1].start_requested);
            assert_eq!(restored.items[1].after, plan.items[1].after);
            assert!(restored.items[1].skip_order.is_empty());
            assert!(!allowed(&db, &input)?);
            assert_eq!(
                scene.journal.read_utterance(input.clone()).await?["state"],
                "cut"
            );
            let history = scene.journal.snapshot().await?["editHistory"].clone();
            scene
                .journal
                .edit_history(crate::timeline_history::Request {
                    op_id: Uuid::now_v7(),
                    action: crate::timeline_history::Action::Redo {
                        entry: history["redo"]["entryId"].as_str().unwrap().into(),
                    },
                })
                .await?;
            scene.journal.advance_plans().await?;
            let queued = scene.journal.plans().await?.pop().unwrap();
            assert!(queued.items[1].start_requested);
            assert_ne!(queued.items[1].utterance_id.unwrap().to_string(), input);
            assert!(queued.items[2].utterance_id.is_none());
            if implicit {
                let input = queued.items[1].utterance_id.unwrap().to_string();
                let turn = start_continuation(&scene, &input).await?;
                assert!(!scene.journal.plans().await?[0].items[1].start_requested);
                finish_parent(
                    &scene,
                    turn,
                    "Result: done — explicitly started dummy child",
                )
                .await?;
                let ended = scene.journal.plans().await?.pop().unwrap();
                assert_eq!(ended.items[0].state, ItemState::Failed);
                assert_eq!(ended.items[1].state, ItemState::Succeeded);
                assert!(ended.items[2].utterance_id.is_none());
            } else {
                scene.journal.pause_dispatch(Uuid::now_v7(), true).await?;
                assert!(!scene.journal.plans().await?[0].items[1].start_requested);
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn retry_item_is_atomic_and_replayed_once_while_other_items_stay_held() -> Result<()> {
        let scene = started_parent(false).await?;
        let mut plan = scene.journal.plans().await?.pop().unwrap();
        assert!(
            scene
                .journal
                .plan_command(command(
                    &plan,
                    Action::RetryItem {
                        item_id: plan.items[0].plan_item_id
                    }
                ))
                .await
                .is_err()
        );
        finish_parent(
            &scene,
            scene.turn,
            "Result: partial — one dummy check remains",
        )
        .await?;
        plan = scene.journal.plans().await?.pop().unwrap();
        plan = scene
            .journal
            .plan_command(command(&plan, Action::Hold))
            .await?;
        assert!(
            scene
                .journal
                .plan_command(command(
                    &plan,
                    Action::RetryItem {
                        item_id: plan.items[1].plan_item_id
                    }
                ))
                .await
                .is_err()
        );
        let request = command(
            &plan,
            Action::RetryItem {
                item_id: plan.items[0].plan_item_id,
            },
        );
        let input = Uuid::new_v5(&request.op_id, b"retry-plan-item").to_string();
        let db = Connection::open(scene.root.path().join("db"))?;
        db.execute_batch("CREATE TRIGGER fail_retry_item BEFORE INSERT ON plan_inputs WHEN NEW.role='continuation' BEGIN SELECT RAISE(ABORT,'dummy save failure'); END;")?;
        assert!(scene.journal.plan_command(request.clone()).await.is_err());
        assert!(scene.journal.read_utterance(input.clone()).await.is_err());
        assert!(scene.journal.operation(request.op_id).await?.is_none());
        assert_eq!(scene.journal.plans().await?[0].rev, plan.rev);
        db.execute_batch("DROP TRIGGER fail_retry_item;")?;
        let retried = scene.journal.plan_command(request.clone()).await?;
        assert_eq!(retried.state, State::Held);
        assert_eq!(retried.items[0].utterance_id, plan.items[0].utterance_id);
        let word = scene.journal.read_utterance(input.clone()).await?;
        assert_eq!(word["planItemId"], plan.items[0].plan_item_id.to_string());
        assert_eq!(word["decision"]["kind"], "extend");
        assert_eq!(word["decision"]["trackId"], scene.track);
        assert_eq!(
            scene.journal.plan_command(request.clone()).await?.rev,
            retried.rev
        );
        let reopened = Journal::open(scene.root.path().join("db"), Key::new([7; 32])).await?;
        assert_eq!(reopened.plan_command(request).await?.rev, retried.rev);
        reopened
            .confirm_fingerprints(reopened.fingerprint_catalog().await?.current)
            .await?;
        let project_id = reopened.snapshot().await?["projectSecrets"]["projectId"]
            .as_str()
            .unwrap()
            .to_owned();
        reopened
            .activate_project_secrets(crate::project_secrets::Load {
                project_id,
                revision: 0,
                entries: vec![],
            })
            .await?;
        assert!(
            scene
                .journal
                .plan_command(command(
                    &retried,
                    Action::RetryItem {
                        item_id: plan.items[0].plan_item_id
                    }
                ))
                .await
                .is_err()
        );
        let before_start = scene.journal.read_utterance(input.clone()).await?;
        assert_eq!(before_start["state"], "placed", "{before_start}");
        assert!(allowed(&db, &input)?, "{before_start}");
        let turn = start_continuation(&scene, &input).await?;
        finish_parent(&scene, turn, "Result: done — dummy check now passes").await?;
        let complete = scene.journal.plans().await?.pop().unwrap();
        assert_eq!(complete.items[0].state, ItemState::Succeeded);
        assert_eq!(complete.state, State::Held);
        assert!(complete.items[1].utterance_id.is_none());
        assert_eq!(
            turn_owner(&db, "dummy-parent-session", &turn.to_string())?,
            Some((plan.id, plan.items[0].plan_item_id))
        );
        Ok(())
    }

    #[tokio::test]
    async fn queued_item_instructions_survive_restart_and_release_dependents_only_after_the_last_turn()
    -> Result<()> {
        let mut scene = started_parent(false).await?;
        finish_parent(
            &scene,
            scene.turn,
            "Result: partial — two dummy details remain",
        )
        .await?;
        let mut inputs = Vec::new();
        for text in [
            "Add the first dummy detail.",
            "Add the second dummy detail.",
        ] {
            let plan = scene.journal.plans().await?.pop().unwrap();
            let input = scene
                .journal
                .stage_document(Submission {
                    op_id: Uuid::now_v7(),
                    text: text.into(),
                    target: crate::decision::Target::Lock(scene.track.clone()),
                    selected_track_id: None,
                    attachment_ids: vec![],
                    sources: None,
                    execute_allowed: true,
                    plan_item: Some(crate::request_sources::PlanItemContext {
                        plan_id: plan.id,
                        item_id: plan.items[0].plan_item_id,
                        revision: plan.rev,
                    }),
                })
                .await?;
            inputs.push(input["id"].as_str().unwrap().to_owned());
        }
        scene.journal = Journal::open(scene.root.path().join("db"), Key::new([7; 32])).await?;
        scene
            .journal
            .confirm_fingerprints(scene.journal.fingerprint_catalog().await?.current)
            .await?;
        let project = scene.journal.snapshot().await?["projectSecrets"]["projectId"]
            .as_str()
            .unwrap()
            .to_owned();
        scene
            .journal
            .activate_project_secrets(crate::project_secrets::Load {
                project_id: project,
                revision: 0,
                entries: vec![],
            })
            .await?;
        let db = Connection::open(scene.root.path().join("db"))?;
        for (index, input) in inputs.iter().enumerate() {
            assert_eq!(
                scene.journal.read_utterance(input.clone()).await?["planItemId"],
                scene.plan.items[0].plan_item_id.to_string()
            );
            assert!(crate::plan_continuation::has_pending(
                &db,
                &scene.plan.items[0].plan_item_id.to_string()
            )?);
            scene.journal.advance_plans().await?;
            assert!(
                scene.journal.plans().await?[0].items[1]
                    .utterance_id
                    .is_none()
            );
            let turn = start_continuation(&scene, input).await?;
            finish_parent(
                &scene,
                turn,
                "Result: done — this dummy instruction is finished",
            )
            .await?;
            scene.journal.advance_plans().await?;
            let plan = scene.journal.plans().await?.pop().unwrap();
            assert_eq!(plan.items[0].utterance_id.unwrap().to_string(), *input);
            assert_eq!(plan.items[0].state, ItemState::Succeeded);
            assert_eq!(
                plan.items[1].utterance_id.is_some(),
                index == inputs.len() - 1
            );
        }
        assert!(!crate::plan_continuation::has_pending(
            &db,
            &scene.plan.items[0].plan_item_id.to_string()
        )?);
        Ok(())
    }

    #[tokio::test]
    async fn prepared_child_applies_the_updated_parent_without_losing_its_own_files() -> Result<()>
    {
        let scene = started_parent(true).await?;
        finish_parent(&scene, scene.turn, "Result: done — original dummy files").await?;
        // Real workers share their folder's object database. This fixture's
        // fake parent uses a separate repository, so expose its dummy objects.
        let info = scene.root.path().join("project/.git/objects/info");
        std::fs::create_dir_all(&info)?;
        std::fs::write(
            info.join("alternates"),
            format!("{}\n", scene.path.join(".git/objects").display()),
        )?;
        scene.journal.advance_plans().await?;
        let before = scene.journal.plans().await?.pop().unwrap();
        let child = &before.items[1];
        let preparation = crate::preparation::Preparation::start(
            &scene.root.path().join("workspaces"),
            scene.journal.clone(),
        )?;
        let prepared = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Ok(workspace) = scene.journal.workspace(child.track_ref.clone()).await {
                    if workspace.state == "ready" {
                        return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(workspace);
                    }
                    if workspace.state == "failed" {
                        return Err(workspace.error.unwrap_or_default().into());
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        preparation.shutdown().await;
        let workspace = prepared??;
        assert_eq!(
            std::fs::read_to_string(workspace.path.join("reference.txt"))?,
            "saved parent\n"
        );
        std::fs::write(workspace.path.join("keep.txt"), "KEEP_CHILD_EDIT\n")?;
        let index = gix::open(&workspace.path)?.git_dir().join("index");
        let original_index = std::fs::read(&index)?;
        let input = scene
            .journal
            .stage_document(Submission {
                op_id: Uuid::now_v7(),
                text: "Update this dummy result.".into(),
                target: crate::decision::Target::Lock(scene.track.clone()),
                selected_track_id: None,
                attachment_ids: vec![],
                sources: None,
                execute_allowed: true,
                plan_item: Some(crate::request_sources::PlanItemContext {
                    plan_id: before.id,
                    item_id: before.items[0].plan_item_id,
                    revision: before.rev,
                }),
            })
            .await?;
        let turn = start_continuation(&scene, input["id"].as_str().unwrap()).await?;
        let updated_tree = {
            let repo = intercat_git::Repository::open(&scene.path)?;
            repo.write_tree(&[("reference.txt", repo.write_blob(b"UPDATED_PARENT\n")?)])?
        };
        finish_parent_tree(
            &scene,
            turn,
            "Result: done — updated dummy files",
            Some(updated_tree),
        )
        .await?;
        scene.journal.advance_plans().await?;
        let plan = scene.journal.plans().await?.pop().unwrap();
        let input = plan.items[1].utterance_id.unwrap().to_string();
        assert_ne!(Some(input.parse::<Uuid>()?), child.utterance_id);
        let db = Connection::open(scene.root.path().join("db"))?;
        assert!(!allowed(&db, &input)?);
        let mut worker = crate::runtime::WorkerState {
            track_id: child.track_ref.clone(),
            phase: "ready".into(),
            session_id: Some("dummy-child-session".into()),
            secret_revision: Some(0),
            identity: None,
            detail: None,
            active_turn: None,
            approvals: vec![],
            output: None,
            checkpoint_pending: false,
            checkpoint: None,
            prompts: vec![],
        };
        scene.journal.worker_state(worker.clone()).await?;
        let idle_host = IdleTestHost::new()?;
        let identity = idle_host.identity()?;
        let boundary = crate::inheritance_apply::Boundary {
            guard: crate::inheritance_apply::Guard::Work(crate::msp::StartGuard::Stopped(
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )),
            tracker: std::sync::Arc::new(std::sync::Mutex::new(crate::process::Tracker::new(
                Uuid::now_v7().to_string(),
                identity,
            )?)),
            identity,
        };
        let applied = crate::inheritance_apply::prepare(
            &scene.journal,
            input.clone(),
            scene.journal.read_utterance(input.clone()).await?["revision"]
                .as_i64()
                .unwrap(),
            "dummy-child-session".into(),
            workspace.clone(),
            None,
            boundary,
        )
        .await?
        .unwrap();
        worker.checkpoint = Some(applied.checkpoint);
        scene.journal.worker_state(worker).await?;
        scene
            .journal
            .change_inheritance(crate::inheritance_apply_store::Change::Complete(
                applied.operation,
            ))
            .await?;
        assert_eq!(
            std::fs::read_to_string(workspace.path.join("reference.txt"))?,
            "UPDATED_PARENT\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.path.join("keep.txt"))?,
            "KEEP_CHILD_EDIT\n"
        );
        assert_eq!(std::fs::read(&index)?, original_index);
        assert!(allowed(&db, &input)?);
        Ok(())
    }

    async fn apply_prepared_child(
        scene: &StartedParent,
        input: &str,
        workspace: &crate::preparation::Workspace,
        worker: &mut crate::runtime::WorkerState,
    ) -> Result<()> {
        let idle_host = IdleTestHost::new()?;
        let identity = idle_host.identity()?;
        let applied = crate::inheritance_apply::prepare(
            &scene.journal,
            input.into(),
            scene.journal.read_utterance(input.into()).await?["revision"]
                .as_i64()
                .unwrap(),
            "dummy-child-session".into(),
            workspace.clone(),
            worker.checkpoint.clone(),
            crate::inheritance_apply::Boundary {
                guard: crate::inheritance_apply::Guard::Work(crate::msp::StartGuard::Stopped(
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                )),
                tracker: std::sync::Arc::new(std::sync::Mutex::new(crate::process::Tracker::new(
                    Uuid::now_v7().to_string(),
                    identity,
                )?)),
                identity,
            },
        )
        .await?
        .ok_or("The dependency replacement was not prepared.")?;
        worker.checkpoint = Some(applied.checkpoint);
        scene.journal.worker_state(worker.clone()).await?;
        scene
            .journal
            .change_inheritance(crate::inheritance_apply_store::Change::Complete(
                applied.operation,
            ))
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn prepared_dependencies_can_be_removed_and_undone_while_held() -> Result<()> {
        let mut scene = started_parent(true).await?;
        finish_parent(&scene, scene.turn, "Result: done — original dummy files").await?;
        let info = scene.root.path().join("project/.git/objects/info");
        std::fs::create_dir_all(&info)?;
        std::fs::write(
            info.join("alternates"),
            format!("{}\n", scene.path.join(".git/objects").display()),
        )?;
        scene.journal.advance_plans().await?;
        let before = scene.journal.plans().await?.pop().unwrap();
        let child = before.items[1].clone();
        let preparation = crate::preparation::Preparation::start(
            &scene.root.path().join("workspaces"),
            scene.journal.clone(),
        )?;
        let prepared = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Ok(workspace) = scene.journal.workspace(child.track_ref.clone()).await {
                    if workspace.state == "ready" {
                        return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(workspace);
                    }
                    if workspace.state == "failed" {
                        return Err(workspace.error.unwrap_or_default().into());
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await;
        preparation.shutdown().await;
        let workspace = prepared??;
        let reference = workspace.path.join("reference.txt");
        assert_eq!(std::fs::read_to_string(&reference)?, "saved parent\n");
        std::fs::write(workspace.path.join("keep.txt"), "KEEP_CHILD_EDIT\n")?;
        let index = gix::open(&workspace.path)?.git_dir().join("index");
        let original_index = std::fs::read(&index)?;
        let held = scene
            .journal
            .plan_command(command(&before, Action::Hold))
            .await?;
        let owner = Uuid::now_v7();
        let token = Uuid::now_v7();
        let edit = scene
            .journal
            .edit_plan(crate::plan_rewrite::Request {
                owner,
                action: crate::plan_rewrite::Action::Begin {
                    plan_id: held.id,
                    item_id: child.plan_item_id,
                    revision: held.rev,
                    token,
                },
            })
            .await?
            .unwrap();
        let changed = scene
            .journal
            .edit_plan(crate::plan_rewrite::Request {
                owner,
                action: crate::plan_rewrite::Action::Dependencies {
                    plan_id: edit.id,
                    revision: edit.rev,
                    token,
                    op_id: Uuid::now_v7(),
                    after: vec![],
                },
            })
            .await?
            .unwrap();
        assert_eq!(changed.state, State::Held);
        scene.journal.advance_plans().await?;
        let changed = scene.journal.plans().await?.pop().unwrap();
        let input = changed.items[1].utterance_id.unwrap().to_string();
        let db = Connection::open(scene.root.path().join("db"))?;
        assert!(!allowed(&db, &input)?);
        assert!(preparable(&db, &input)?);
        assert!(crate::plan_files::pending(&db, &input)?.is_some());
        let mut worker = crate::runtime::WorkerState {
            track_id: child.track_ref.clone(),
            phase: "ready".into(),
            session_id: Some("dummy-child-session".into()),
            secret_revision: Some(0),
            identity: None,
            detail: None,
            active_turn: None,
            approvals: vec![],
            output: None,
            checkpoint_pending: false,
            checkpoint: None,
            prompts: vec![],
        };
        scene.journal.worker_state(worker.clone()).await?;
        std::fs::write(&reference, "CHILD_CHANGED_INHERITED_FILE\n")?;
        let error = apply_prepared_child(&scene, &input, &workspace, &mut worker)
            .await
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<crate::inheritance_issue::Blocker>(),
            Some(crate::inheritance_issue::Blocker::Conflict { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&reference)?,
            "CHILD_CHANGED_INHERITED_FILE\n"
        );
        assert_eq!(std::fs::read(&index)?, original_index);
        std::fs::write(&reference, "saved parent\n")?;
        apply_prepared_child(&scene, &input, &workspace, &mut worker).await?;
        assert!(!reference.exists());
        assert_eq!(
            std::fs::read_to_string(workspace.path.join("keep.txt"))?,
            "KEEP_CHILD_EDIT\n"
        );
        assert_eq!(std::fs::read(&index)?, original_index);
        assert!(!allowed(&db, &input)?);
        assert!(crate::plan_files::pending(&db, &input)?.is_none());
        assert!(!crate::plan_files::has_files(
            &db,
            child.plan_item_id,
            &child.track_ref
        )?);
        // The persisted origin, rather than process memory, drives Undo.
        scene.journal = Journal::open(scene.root.path().join("db"), Key::new([7; 32])).await?;
        let snapshot = scene.journal.snapshot().await?;
        scene
            .journal
            .edit_history(crate::timeline_history::Request {
                op_id: Uuid::now_v7(),
                action: crate::timeline_history::Action::Undo {
                    entry: snapshot["editHistory"]["undo"]["entryId"]
                        .as_str()
                        .unwrap()
                        .into(),
                },
            })
            .await?;
        let undone = scene.journal.plans().await?.pop().unwrap();
        assert_eq!(undone.items[1].after, child.after);
        scene.journal.advance_plans().await?;
        let undone = scene.journal.plans().await?.pop().unwrap();
        assert_eq!(undone.state, State::Held);
        let undo_input = undone.items[1].utterance_id.unwrap().to_string();
        assert!(preparable(&db, &undo_input)?);
        assert!(!allowed(&db, &undo_input)?);
        apply_prepared_child(&scene, &undo_input, &workspace, &mut worker).await?;
        assert_eq!(std::fs::read_to_string(&reference)?, "saved parent\n");
        assert_eq!(
            std::fs::read_to_string(workspace.path.join("keep.txt"))?,
            "KEEP_CHILD_EDIT\n"
        );
        assert_eq!(std::fs::read(&index)?, original_index);
        assert!(!allowed(&db, &undo_input)?);
        assert!(crate::plan_files::pending(&db, &undo_input)?.is_none());
        assert!(crate::plan_files::has_files(
            &db,
            child.plan_item_id,
            &child.track_ref
        )?);
        // Revising words after preparation retains the same file recipe and
        // waits for acceptance without changing the working tree.
        let current = scene.journal.plans().await?.pop().unwrap();
        let owner = Uuid::now_v7();
        let token = Uuid::now_v7();
        let edit = scene
            .journal
            .edit_plan(crate::plan_rewrite::Request {
                owner,
                action: crate::plan_rewrite::Action::Begin {
                    plan_id: current.id,
                    item_id: child.plan_item_id,
                    token,
                    revision: current.rev,
                },
            })
            .await?
            .unwrap();
        assert!(!preparable(&db, &undo_input)?);
        let submitted = scene
            .journal
            .edit_plan(crate::plan_rewrite::Request {
                owner,
                action: crate::plan_rewrite::Action::Submit {
                    plan_id: edit.id,
                    token,
                    revision: edit.rev,
                    op_id: Uuid::now_v7(),
                    text: "Keep the independent dummy file while completing this item.".into(),
                },
            })
            .await?
            .unwrap();
        assert!(
            scene
                .journal
                .begin_plan(submitted.id, submitted.generation.id, submitted.rev)
                .await?
        );
        let revised = format!("{} Preserve keep.txt.", child.instruction);
        assert!(
            scene
                .journal
                .generated_plan(
                    submitted.id,
                    submitted.generation.id,
                    submitted.rev,
                    Generated::Draft(crate::plans::Draft {
                        assumptions: vec![],
                        conflicts: vec![],
                        summary: submitted.summary.clone(),
                        items: vec![crate::plans::DraftItem {
                            id: child.plan_item_id.to_string(),
                            track_ref: child.track_ref.clone(),
                            folder_id: child.folder_id.clone(),
                            title: child.title.clone(),
                            instruction: revised.clone(),
                            constraints: child.constraints.clone(),
                            done_when: child.done_when.clone(),
                            after: child.after.clone(),
                        }],
                    }),
                    vec![]
                )
                .await?
        );
        let ready = scene.journal.plans().await?.pop().unwrap();
        assert_eq!(ready.items[1].state, ItemState::Editing);
        assert_eq!(ready.items[1].instruction, child.instruction);
        let accepted = scene
            .journal
            .edit_plan(crate::plan_rewrite::Request {
                owner,
                action: crate::plan_rewrite::Action::Apply {
                    plan_id: ready.id,
                    token,
                    revision: ready.rev,
                    op_id: Uuid::now_v7(),
                },
            })
            .await?
            .unwrap();
        assert_eq!(accepted.state, State::Held);
        assert_eq!(accepted.items[1].instruction, revised);
        scene.journal.advance_plans().await?;
        let accepted = scene.journal.plans().await?.pop().unwrap();
        let input = accepted.items[1].utterance_id.unwrap().to_string();
        assert!(!allowed(&db, &input)?);
        assert!(crate::plan_files::pending(&db, &input)?.is_none());
        let snapshot = scene.journal.snapshot().await?;
        scene
            .journal
            .edit_history(crate::timeline_history::Request {
                op_id: Uuid::now_v7(),
                action: crate::timeline_history::Action::Undo {
                    entry: snapshot["editHistory"]["undo"]["entryId"]
                        .as_str()
                        .unwrap()
                        .into(),
                },
            })
            .await?;
        assert_eq!(
            scene.journal.plans().await?[0].items[1].instruction,
            child.instruction
        );
        assert_eq!(std::fs::read_to_string(&reference)?, "saved parent\n");
        assert_eq!(
            std::fs::read_to_string(workspace.path.join("keep.txt"))?,
            "KEEP_CHILD_EDIT\n"
        );
        assert_eq!(std::fs::read(&index)?, original_index);
        Ok(())
    }

    #[tokio::test]
    async fn a_new_parent_result_replaces_the_unstarted_child_reservation() -> Result<()> {
        let scene = started_parent(true).await?;
        finish_parent(&scene, scene.turn, "Result: done — first dummy result").await?;
        scene.journal.advance_plans().await?;
        let before = scene.journal.plans().await?.pop().unwrap();
        let child = &before.items[1];
        let old_input = child.utterance_id.unwrap();
        let db = Connection::open(scene.root.path().join("db"))?;
        crate::clock::register(&db)?;
        let old_dependencies =
            crate::inheritance::dependencies(&db, &old_input.to_string())?.unwrap();
        let parent = &old_dependencies.points[0];
        let mut basis = parent.basis();
        crate::lineage_store::freeze(&db)?.incorporate(&mut basis.inherits, parent)?;
        crate::lineage_store::initialize(&db, &child.track_ref, &basis)?;
        crate::lineage_store::set_basis(&db, &child.track_ref, &basis)?;
        assert!(allowed(&db, &old_input.to_string())?);
        let initial = crate::inheritance::Initial {
            operation: Uuid::now_v7(),
            dependencies: old_dependencies.clone(),
            head: parent.sha.clone(),
            basis,
            original: crate::lineage::Basis::root(parent.base_at.clone()),
        };
        scene
            .journal
            .intent(
                &initial.operation.to_string(),
                "workspace.inherit",
                serde_json::to_value(&initial)?,
            )
            .await?;
        scene
            .journal
            .finish(&initial.operation.to_string(), "dispatched")
            .await?;
        scene
            .journal
            .finish(&initial.operation.to_string(), "complete")
            .await?;
        let id = Uuid::now_v7();
        scene
            .journal
            .stage_utterance(
                id,
                "Update this dummy item.".into(),
                crate::decision::Target::Lock(scene.track.clone()),
                None,
                vec![],
            )
            .await?;
        let input = scene.journal.claim_input().await?.unwrap();
        scene
            .journal
            .apply_input(
                input.id.clone(),
                input.revision,
                crate::decision::Outcome::Classified(crate::decision::Decision {
                    kind: crate::decision::Kind::Extend,
                    track_id: Some(scene.track.clone()),
                    boundary: Some("after-turn".into()),
                    command: None,
                    confidence: 1.0,
                    inference_ms: 0.0,
                    timed_out: false,
                    scores: json!({}),
                }),
            )
            .await?;
        crate::plan_continuation::attach(&db, &input.id, before.id, before.items[0].plan_item_id)?;
        assert!(!allowed(&db, &old_input.to_string())?);
        let next_turn = start_continuation(&scene, &input.id).await?;
        finish_parent(&scene, next_turn, "Result: done — updated dummy result").await?;
        // Admission must reject the old reservation before the scheduler refreshes it.
        assert!(!allowed(&db, &old_input.to_string())?);
        assert!(!preparable(&db, &old_input.to_string())?);
        scene.journal.advance_plans().await?;
        let after = scene.journal.plans().await?.pop().unwrap();
        let new_input = after.items[1].utterance_id.unwrap();
        assert_ne!(new_input, old_input);
        assert_eq!(
            scene.journal.read_utterance(old_input.to_string()).await?["state"],
            "cut"
        );
        let dependencies = crate::inheritance::dependencies(&db, &new_input.to_string())?.unwrap();
        assert_eq!(dependencies.points[0].number, parent.number + 1);
        assert_ne!(dependencies.points[0].sha, parent.sha);
        assert!(!allowed(&db, &new_input.to_string())?);
        assert!(preparable(&db, &new_input.to_string())?);
        assert!(
            crate::plan_edit::editable(&db, &after.items[1]).is_err(),
            "Older file preparation remains attached to this item"
        );
        let application =
            crate::inheritance_apply_store::context(&db, &new_input.to_string())?.unwrap();
        assert_eq!(application.source.dependencies, dependencies);
        let mut latest_basis = dependencies.points[0].basis();
        crate::lineage_store::freeze(&db)?
            .incorporate(&mut latest_basis.inherits, &dependencies.points[0])?;
        crate::lineage_store::set_basis(&db, &after.items[1].track_ref, &latest_basis)?;
        assert!(allowed(&db, &new_input.to_string())?);
        Ok(())
    }

    #[tokio::test]
    async fn cross_folder_queue_pins_a_saved_reference_without_inheriting_its_files() -> Result<()>
    {
        let StartedParent {
            root,
            journal,
            mut plan,
            track,
            path,
            sha,
            turn,
        } = started_parent(false).await?;
        // Supply an already admitted two-folder plan. This exercises the
        // execution binding; project-folder selection has its own UI scope.
        plan.items[0].state = ItemState::Succeeded;
        plan.items[1].folder_id = "second-folder".into();
        let db = Connection::open(root.path().join("db"))?;
        crate::clock::register(&db)?;
        crate::plan_store::save(&db, &plan, "fixture.completed-parent")?;
        journal.try_msp(track.clone(), "dummy-parent-session".into(), crate::clock::Stamp::now(),
            json!({"method":"item/completed","params":{"sessionId":"dummy-parent-session","item":{
                "itemId":"dummy-success-report","turnId":turn,"kind":"agentMessage","messagePhase":"finalAnswer","status":"completed","text":"Result: done — dummy work exists","revision":1
            }}}),None,None)?.await??;
        journal.try_msp(track.clone(), "dummy-parent-session".into(), crate::clock::Stamp::now(),
            json!({"method":"turn/completed","params":{"sessionId":"dummy-parent-session","turnId":turn,"terminal":"completed"}}),None,None)?.await??;
        journal.advance_plans().await?;
        let waiting = journal.plans().await?.pop().unwrap();
        assert!(waiting.items[1].utterance_id.is_none());
        assert!(
            waiting.items[1]
                .detail
                .as_deref()
                .unwrap()
                .contains("saved files")
        );
        let checkpoint = crate::checkpoint::Checkpoint {
            turn_id: turn,
            sha: sha.to_string(),
            reference: format!("refs/intercat/test/tracks/A-checkpoints/{turn}"),
            unmerged_paths: vec![],
            live_processes: false,
        };
        let checkpoint_id = Uuid::new_v5(&turn, b"checkpoint");
        journal
            .intent(
                &checkpoint_id.to_string(),
                "checkpoint.save",
                json!({"trackId":track,"checkpoint":checkpoint}),
            )
            .await?;
        journal
            .finish(&checkpoint_id.to_string(), "dispatched")
            .await?;
        journal
            .finish(&checkpoint_id.to_string(), "complete")
            .await?;
        journal.advance_plans().await?;
        let ready = journal.plans().await?.pop().unwrap();
        let child = &ready.items[1];
        let child_input = child.utterance_id.unwrap();
        let recipe =
            crate::dependency_references::read(&db, &child_input.to_string(), &child.track_ref)?
                .unwrap();
        assert_eq!(recipe.sources.len(), 1);
        assert_eq!(recipe.sources[0].checkpoint, sha.to_string());
        assert_eq!(recipe.sources[0].track, track);
        assert_eq!(recipe.sources[0].repository, path);
        let displayed = journal.snapshot().await?["planDependencies"]
            [ready.items[0].plan_item_id.to_string()]
        .clone();
        assert_eq!(displayed["trackId"], track);
        assert_eq!(displayed["state"], "succeeded");
        assert!(displayed["clipId"].is_string());
        assert!(displayed["atMs"].is_i64());
        let window = journal
            .timeline_window(crate::timeline::Window {
                tracks: vec![track.clone()],
                start: 0,
                end: i64::MAX,
                since: None,
            })
            .await?;
        assert!(window["clips"].as_array().unwrap().iter().any(|clip| {
            clip["turnId"] == turn.to_string()
                && clip["planId"] == ready.id.to_string()
                && clip["planItemId"] == ready.items[0].plan_item_id.to_string()
        }));
        assert!(
            journal
                .input_inheritance(child_input.to_string())
                .await?
                .is_none()
        );
        for terminal in ["cancelled", "failed", "completed"] {
            let mut checking = ready.clone();
            checking.items[0].state = ItemState::Running;
            crate::plan_store::save(&db, &checking, "fixture.check-terminal")?;
            db.execute("INSERT INTO events(ts,session_id,kind,payload) VALUES(0,'dummy-parent-session','msp.applied',?1)",[json!({"method":"turn/completed","params":{"sessionId":"dummy-parent-session","turnId":turn,"terminal":terminal}}).to_string()])?;
            journal.advance_plans().await?;
            let checked = journal.plans().await?.pop().unwrap();
            assert_eq!(
                checked.items[0].state,
                if terminal == "completed" {
                    ItemState::Succeeded
                } else {
                    ItemState::Failed
                }
            );
            assert_eq!(
                allowed(&db, &child_input.to_string())?,
                terminal == "completed"
            );
        }
        journal
            .observe_report(crate::reports::Report::new(
                track,
                "dummy-parent-session".into(),
                turn,
                "dummy-bound-question".into(),
                "Waiting for you: Which dummy output format should I use?".into(),
            ))
            .await?;
        let questions = journal.snapshot().await?["reportQuestions"].clone();
        assert!(questions.as_array().unwrap().iter().any(|question| {
            question["turnId"] == turn.to_string()
                && question["planItemId"] == ready.items[0].plan_item_id.to_string()
        }));
        Ok(())
    }
    #[tokio::test]
    async fn external_clip_waits_for_its_own_result_save_and_questions_then_pins_both_file_paths()
    -> Result<()> {
        let (root, journal, old_plan) =
            fixture_layout(false, Some("UNUSED_DUMMY_MARKER"), false).await?;
        let db = Connection::open(root.path().join("db"))?;
        crate::clock::register(&db)?;
        let project: crate::project::Project =
            serde_json::from_value(journal.snapshot().await?["project"].clone())?;
        let track = Uuid::now_v7().to_string();
        db.execute(
            "INSERT INTO tracks VALUES(?1,'A','Dummy external source','done')",
            [&track],
        )?;
        crate::folders::bind(&db, &track, &project.folders[0].id())?;
        let path = root.path().join("external-source");
        std::fs::create_dir(&path)?;
        gix::init(&path)?;
        let repo = intercat_git::Repository::open(&path)?;
        let base = repo.empty_starting_point()?;
        let sha = repo.checkpoint_commit(
            repo.write_tree(&[("saved.txt", repo.write_blob(b"SAVED_DUMMY\n")?)])?,
            base,
            "Dummy external checkpoint",
        )?;
        journal
            .workspace_result(crate::preparation::Workspace {
                denied_roots: Vec::new(),
                track_id: track.clone(),
                path: path.clone(),
                reference: "refs/intercat/test/tracks/A".into(),
                base: base.to_string(),
                state: "ready".into(),
                error: None,
                attachment_roots: vec![],
            })
            .await?;
        let clip = Uuid::now_v7().to_string();
        let turn = Uuid::now_v7();
        let session = "external-dummy-session";
        let stamp = crate::clock::Stamp::now();
        db.execute("INSERT INTO timeline_clips VALUES(?1,?2,1,?3)", params![clip,track,json!({
            "id":clip,"trackId":track,"repairRunId":null,"ordinal":1,"sessionId":session,"turnId":turn,
            "start":stamp,"end":stamp,"generationEnd":stamp,"category":"step","shellItem":null,"recovered":false,
        }).to_string()])?;
        db.execute("INSERT INTO timeline_turns VALUES(?1,?2,?3)",params![session,turn.to_string(),json!({"track":track,"repair_run_id":null,"current":null,"index":1,"terminal":false}).to_string()])?;
        let report = |text: &str| -> Result<()> {
            db.execute("INSERT INTO timeline_items VALUES(?1,'external-report',?2,1,?3,'{}') ON CONFLICT(session,item) DO UPDATE SET revision=revision+1,record=excluded.record",params![session,clip,json!({"kind":"agentMessage","messagePhase":"finalAnswer","turnId":turn,"status":"completed","text":text}).to_string()])?;
            Ok(())
        };
        report("Result: done — saved dummy result")?;
        let plan = journal
            .plan_command(command(&old_plan, Action::Retry))
            .await?;
        journal
            .begin_plan(plan.id, plan.generation.id, plan.rev)
            .await?;
        journal
            .generated_plan(
                plan.id,
                plan.generation.id,
                plan.rev,
                Generated::Draft(Draft {
                    assumptions: vec![],
                    conflicts: vec![],
                    summary: "Use an existing dummy result in two folders".into(),
                    items: project
                        .folders
                        .iter()
                        .enumerate()
                        .map(|(i, folder)| DraftItem {
                            id: format!("child-{i}"),
                            track_ref: format!("new:{}", i + 1),
                            folder_id: folder.id(),
                            title: format!("Child {i}"),
                            instruction: "Read saved.txt and create a dummy result.".into(),
                            constraints: vec![],
                            done_when: "A dummy result exists.".into(),
                            after: vec![crate::plans::Dependency {
                                id: clip.clone(),
                                inherit: true,
                            }],
                        })
                        .collect(),
                }),
                vec![],
            )
            .await?;
        let plan = journal.plans().await?.pop().unwrap();
        assert!(plan.items[0].after[0].inherit);
        assert!(!plan.items[1].after[0].inherit);
        journal.plan_command(command(&plan, Action::Review)).await?;
        journal.plan_command(command(&plan, Action::Start)).await?;
        journal.advance_plans().await?;
        assert!(
            journal.plans().await?[0]
                .items
                .iter()
                .all(|item| item.utterance_id.is_none())
        );
        db.execute("UPDATE timeline_turns SET record=json_set(record,'$.terminal',1) WHERE session=?1 AND turn=?2",params![session,turn.to_string()])?;
        db.execute("INSERT INTO events(ts,session_id,kind,payload) VALUES(0,?1,'msp.applied',?2)",params![session,json!({"method":"turn/completed","params":{"sessionId":session,"turnId":turn,"terminal":"completed"}}).to_string()])?;
        for terminal in ["cancelled", "failed", "unknown"] {
            db.execute("UPDATE events SET payload=json_set(payload,'$.params.terminal',?1) WHERE session_id=?2 AND kind='msp.applied'",params![terminal,session])?;
            journal.advance_plans().await?;
            let current = journal.plans().await?.pop().unwrap();
            assert!(current.items.iter().all(|item| {
                item.utterance_id.is_none()
                    && item
                        .detail
                        .as_ref()
                        .is_some_and(|detail| detail.contains("didn’t finish"))
            }));
        }
        db.execute("UPDATE events SET payload=json_set(payload,'$.params.terminal','completed') WHERE session_id=?1 AND kind='msp.applied'",[session])?;
        for (text, reason) in [
            ("Result: partial — not finished", "didn’t finish"),
            ("Result: failed — not finished", "didn’t finish"),
            ("The dummy turn ended without a result.", "result unclear"),
            ("Result: done — saved dummy result", "saved files"),
        ] {
            report(text)?;
            journal.advance_plans().await?;
            let current = journal.plans().await?.pop().unwrap();
            assert!(current.items.iter().all(|item| item.utterance_id.is_none()));
            let displayed = journal.snapshot().await?["planDependencies"][&clip].clone();
            assert_eq!(displayed["trackId"], track);
            assert_eq!(displayed["clipId"], clip);
            assert_eq!(displayed["atMs"], stamp.timeline_ms);
            assert_eq!(
                displayed["state"],
                if reason == "saved files" {
                    "succeeded"
                } else {
                    "failed"
                }
            );
            assert_eq!(displayed["unclear"], reason == "result unclear");
            assert!(current.items.iter().all(|item| {
                item.detail
                    .as_ref()
                    .is_some_and(|detail| detail.contains(reason))
            }));
        }
        let checkpoint = crate::checkpoint::Checkpoint {
            turn_id: turn,
            sha: sha.to_string(),
            reference: format!("refs/intercat/test/checkpoints/{turn}"),
            unmerged_paths: vec![],
            live_processes: false,
        };
        let save = Uuid::new_v5(&turn, b"checkpoint").to_string();
        journal
            .intent(
                &save,
                "checkpoint.save",
                json!({"trackId":track,"checkpoint":checkpoint}),
            )
            .await?;
        journal.finish(&save, "dispatched").await?;
        journal.finish(&save, "complete").await?;
        // A newer, unrelated turn must neither satisfy nor invalidate this clip.
        let newer = Uuid::now_v7();
        let newer_clip = Uuid::now_v7().to_string();
        db.execute("INSERT INTO timeline_clips VALUES(?1,?2,2,?3)",params![newer_clip,track,json!({"id":newer_clip,"trackId":track,"sessionId":session,"turnId":newer,"repairRunId":null}).to_string()])?;
        db.execute(
            "INSERT INTO timeline_turns VALUES(?1,?2,?3)",
            params![
                session,
                newer.to_string(),
                json!({"terminal":true}).to_string()
            ],
        )?;
        db.execute("INSERT INTO timeline_items VALUES(?1,'newer-report',?2,1,?3,'{}')",params![session,newer_clip,json!({"kind":"agentMessage","messagePhase":"finalAnswer","turnId":newer,"status":"completed","text":"Result: done — unrelated turn"}).to_string()])?;
        report("Result: failed — original turn is still failed")?;
        journal.advance_plans().await?;
        assert!(
            journal.plans().await?[0]
                .items
                .iter()
                .all(|item| item.utterance_id.is_none())
        );
        report("Result: done — saved dummy result")?;
        db.execute("UPDATE timeline_items SET record=json_set(record,'$.text','Result: failed — unrelated turn') WHERE session=?1 AND item='newer-report'", [session])?;
        let pending_report = crate::reports::Report::new(
            track.clone(),
            session.into(),
            turn,
            "external-report".into(),
            "Does the dummy result look correct?".into(),
        );
        db.execute(
            "INSERT INTO reports VALUES(?1,?2,1,?3)",
            params![
                pending_report.id,
                track,
                serde_json::to_string(&pending_report)?
            ],
        )?;
        journal.advance_plans().await?;
        let waiting = journal.plans().await?.pop().unwrap();
        assert!(waiting.items.iter().all(|item| {
            item.utterance_id.is_none()
                && item
                    .detail
                    .as_ref()
                    .is_some_and(|detail| detail.contains("answer"))
        }));
        db.execute(
            "UPDATE reports SET pending=0 WHERE id=?1",
            [&pending_report.id],
        )?;
        let question = crate::reports::Question {
            id: Uuid::now_v7().to_string(),
            report_id: pending_report.id.clone(),
            track_id: track.clone(),
            session_id: session.into(),
            turn_id: turn,
            revision: 1,
            kind: "plain".into(),
            text: "Dummy question".into(),
            state: "open".into(),
            assumed: None,
            reply_utterance_id: None,
        };
        db.execute(
            "INSERT INTO report_questions VALUES(?1,?2,?3,'open',?4)",
            params![
                question.id,
                question.report_id,
                track,
                serde_json::to_string(&question)?
            ],
        )?;
        journal.advance_plans().await?;
        assert!(
            journal.plans().await?[0]
                .items
                .iter()
                .all(|item| item.utterance_id.is_none())
        );
        db.execute(
            "UPDATE report_questions SET state='answered' WHERE id=?1",
            [&question.id],
        )?;
        journal.advance_plans().await?;
        let ready = journal.plans().await?.pop().unwrap();
        let inherited = journal
            .input_inheritance(ready.items[0].utterance_id.unwrap().to_string())
            .await?
            .unwrap();
        assert_eq!(inherited.dependencies.points[0].sha, sha.to_string());
        assert_eq!(inherited.dependencies.points[0].number, 1);
        let child = &ready.items[1];
        let reference = crate::dependency_references::read(
            &db,
            &child.utterance_id.unwrap().to_string(),
            &child.track_ref,
        )?
        .unwrap();
        assert_eq!(reference.sources[0].checkpoint, sha.to_string());
        assert_eq!(reference.sources[0].repository, path);
        assert_eq!(reference.sources[0].number, 1);
        assert!(
            journal
                .input_inheritance(child.utterance_id.unwrap().to_string())
                .await?
                .is_none()
        );
        assert!(
            crate::plan_dependencies::read(&db, std::slice::from_ref(&ready), &newer_clip)?
                .unwrap()
                .state
                == ItemState::Failed
        );
        db.execute("DELETE FROM timeline_clips WHERE id=?1", [&clip])?;
        assert!(!preparable(&db, &child.utterance_id.unwrap().to_string())?);
        // The request retains its exact saved reference even after the source is unavailable.
        assert_eq!(
            crate::dependency_references::read(
                &db,
                &child.utterance_id.unwrap().to_string(),
                &child.track_ref
            )?
            .unwrap(),
            reference
        );
        Ok(())
    }
    #[tokio::test]
    async fn track_order_and_explicit_dependencies_require_success_not_other_work() -> Result<()> {
        let (root, _journal, mut plan) = fixture(false).await?;
        let db = Connection::open(root.path().join("db"))?;
        let initial = vec![plan.clone()];
        assert!(waiting(&db, &plan.items[0], &plan, &initial)?.is_none());
        assert!(waiting(&db, &plan.items[1], &plan, &initial)?.is_some());
        assert!(waiting(&db, &plan.items[2], &plan, &initial)?.is_some());
        plan.items[0].state = ItemState::Failed;
        assert!(
            waiting(&db, &plan.items[1], &plan, &[plan.clone()])?
                .unwrap()
                .contains("didn’t finish")
        );
        plan.items[0].state = ItemState::Succeeded;
        assert!(waiting(&db, &plan.items[1], &plan, &[plan.clone()])?.is_none());
        assert!(waiting(&db, &plan.items[2], &plan, &[plan.clone()])?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn retiring_history_invalidates_its_result_and_holds_the_remaining_plan() -> Result<()> {
        let (root, journal, plan) = fixture(false).await?;
        journal.plan_command(command(&plan, Action::Review)).await?;
        journal.plan_command(command(&plan, Action::Start)).await?;
        journal.advance_plans().await?;
        let mut plan = journal.plans().await?.pop().unwrap();
        let first = plan.items[0].utterance_id.unwrap().to_string();
        let mut db = Connection::open(root.path().join("db"))?;
        db.pragma_update(None, "foreign_keys", true)?;
        crate::clock::register(&db)?;
        // Completing the first item lets the next item on its track queue.
        plan.items[0].state = ItemState::Succeeded;
        crate::plan_store::save(&db, &plan, "dummy.completed")?;
        journal.advance_plans().await?;
        let plan = journal.plans().await?.pop().unwrap();
        let next = plan.items[2].utterance_id.unwrap().to_string();
        db.execute(
            "INSERT INTO timeline_marks VALUES('old-session','dummy-mark',?1,0,?2,NULL)",
            params![plan.items[0].track_ref, first],
        )?;
        let operation = Uuid::now_v7().to_string();
        db.execute("INSERT INTO operations VALUES(?1,'workspace.apply-inheritance',?2,'dispatched')", params![operation,json!({"session":"old-session","dependencies":{"track":plan.items[0].track_ref,"input":first}}).to_string()])?;
        let tx = db.transaction()?;
        crate::session_retirement::retire(
            &tx,
            "old-session",
            &plan.items[0].track_ref,
            Uuid::now_v7(),
        )?;
        tx.commit()?;
        let held = journal.plans().await?.pop().unwrap();
        assert_eq!(held.state, State::Held);
        assert!(held.rev > plan.rev);
        assert_eq!(held.items[0].state, ItemState::Failed);
        assert!(journal.read_utterance(first.clone()).await.is_err());
        assert!(owner(&db, &first)?.is_none());
        assert!(!allowed(&db, &next)?);
        assert_eq!(journal.read_utterance(next.clone()).await?["state"], "held");
        assert!(journal.operation(operation.parse()?).await?.is_none());
        journal.advance_plans().await?;
        assert_eq!(journal.read_utterance(next.clone()).await?["state"], "held");
        // If the source itself is retired, remove the plan and its links while
        // retaining the held state of any remaining, non-retired requests.
        db.execute(
            "INSERT INTO timeline_marks VALUES('source-session','source-mark',?1,1,?2,NULL)",
            params![held.items[0].track_ref, held.source_id],
        )?;
        let tx = db.transaction()?;
        crate::session_retirement::retire(
            &tx,
            "source-session",
            &held.items[0].track_ref,
            Uuid::now_v7(),
        )?;
        tx.commit()?;
        assert!(journal.plans().await?.is_empty());
        assert!(owner(&db, &next)?.is_none());
        assert_eq!(journal.read_utterance(next).await?["state"], "held");
        Ok(())
    }

    #[tokio::test]
    #[ignore = "three paid dummy turns: partial item, explicit Retry, then its dependent"]
    async fn real_failed_item_retries_once_before_its_dependent_runs() -> Result<()> {
        real_plan(false, ChildWorkspace::RetryItem).await
    }

    #[tokio::test]
    #[ignore = "two paid dummy worker turns; requires saved auth and INTERCAT_M3_START_OUTPUT"]
    async fn real_reviewed_plan_runs_two_dummy_items_after_start() -> Result<()> {
        real_plan(false, ChildWorkspace::New).await
    }
    #[tokio::test]
    #[ignore = "two paid dummy worker turns started by a reviewed plan's spoken go-ahead"]
    async fn real_spoken_go_starts_a_plan_only_after_its_review() -> Result<()> {
        real_plan(false, ChildWorkspace::SpokenGo).await
    }
    #[tokio::test]
    #[ignore = "one paid dummy worker turn after an injected Planner timeout"]
    async fn real_single_job_after_planner_timeout_runs_only_the_original_request() -> Result<()> {
        let (root, journal, untouched) = fixture(false).await?;
        let output = std::path::PathBuf::from(std::env::var("INTERCAT_M3_SINGLE_OUTPUT")?);
        std::fs::create_dir_all(&output)?;
        journal
            .set_worker_config(
                Uuid::now_v7(),
                crate::runtime::Config {
                    source: std::env::var("INTERCAT_MUSE_BINARY")?.into(),
                    binary: std::env::var("INTERCAT_MUSE_BINARY")?.into(),
                    credentials: std::env::var("INTERCAT_MUSE_AUTH")?.into(),
                    path: std::env::var("PATH")?,
                },
            )
            .await?;
        let rpc = root.path().join("rpc");
        std::fs::create_dir(&rpc)?;
        let preparation = crate::preparation::Preparation::start(
            &root.path().join("workspaces"),
            journal.clone(),
        )?;
        let editor = ready_editor(root.path()).await?;
        let pool = std::sync::Arc::new(crate::scheduler::Capacity::new(1));
        let runtime = crate::runtime::Runtime::start(
            crate::host_memory::Pool::new(),
            root.path().into(),
            rpc,
            journal.clone(),
            editor.clone(),
            pool.clone(),
            std::sync::Arc::new(crate::dispatch_control::Gate::new(false)),
        );
        let result = async {
            let project_id = journal.snapshot().await?["projectSecrets"]["projectId"].as_str().unwrap().to_owned();
            journal.confirm_fingerprints(journal.fingerprint_catalog().await?.current).await?;
            let key = Key::new([7; 32]);
            let catalog = journal.prepare_project_secrets(crate::project_secrets::Prepare {
                op_id: Uuid::now_v7(), project_id: project_id.clone(), revision: 0, fingerprints: vec![],
                key_check: key.check().iter().map(|byte| format!("{byte:02x}")).collect(),
            }).await?;
            journal.confirm_fingerprints(catalog.current).await?;
            runtime.service.activate_project_secrets(crate::project_secrets::Load { project_id, revision: 0, entries: vec![] }).await?;
            let source = Uuid::now_v7();
            journal.stage_utterance(source, "This is a disposable dummy test. Create single.txt containing exactly DUMMY_ONE_JOB followed by a newline. Do not edit other files, use the network, ask questions, run commands or make commits. Report Result: done when the file is saved.".into(), crate::decision::Target::New, None, vec![]).await?;
            let initial = journal.claim_input().await?.unwrap();
            journal.apply_input(initial.id, initial.revision, crate::decision::Outcome::Classified(crate::decision::Decision { kind: crate::decision::Kind::Plan, track_id: None, boundary: None, command: None, confidence: 1., inference_ms: 0., timed_out: false, scores: json!({}) })).await?;
            let plan = journal.plans().await?.into_iter().find(|plan| plan.source_id == source.to_string()).unwrap();
            journal.begin_plan(plan.id, plan.generation.id, plan.rev).await?;
            journal.generated_plan(plan.id, plan.generation.id, plan.rev, Generated::TimedOut, vec![]).await?;
            let failed = journal.plans().await?.into_iter().find(|p| p.id == plan.id).unwrap();
            assert!(journal.snapshot().await?["workers"].as_array().unwrap().is_empty());
            let fallback = journal.plan_command(command(&failed, Action::RunAsOne)).await?;
            assert_eq!(fallback.state, State::SingleJob);
            let input = journal.claim_input().await?.unwrap();
            assert!(input.single_job); assert_eq!(input.id, source.to_string());
            journal.apply_input(input.id, input.revision, crate::decision::Outcome::Classified(crate::decision::Decision { kind: crate::decision::Kind::Fork, track_id: None, boundary: Some("now".into()), command: None, confidence: 1., inference_ms: 0., timed_out: false, scores: json!({}) })).await?;
            let final_state = tokio::time::timeout(std::time::Duration::from_secs(120), async {
                loop {
                    let snapshot = journal.snapshot().await?;
                    std::fs::write(output.join("latest.json"), serde_json::to_vec_pretty(&snapshot)?)?;
                    for report in snapshot["reports"].as_array().unwrap() {
                        journal.analyze_report(report["id"].as_str().unwrap().into(), report["revision"].as_i64().unwrap(), crate::reports::Analysis::Clear).await?;
                    }
                    if let Some(saved) = journal.read_input_origin(source).await?
                        && matches!(saved.phase, crate::input_origin::Phase::Finished | crate::input_origin::Phase::MovedOn)
                        && saved.checkpoint.is_some() { break Ok::<_, Box<dyn std::error::Error + Send + Sync>>(snapshot); }
                    if let Some(worker) = snapshot["workers"].as_array().unwrap().iter().find(|worker| worker["phase"] == "held" && worker["detail"].is_string()) {
                        return Err(format!("Dummy worker held: {}", worker["detail"]).into());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }).await??;
            assert_eq!(final_state["workers"].as_array().unwrap().len(), 1);
            let track = final_state["workers"][0]["trackId"].as_str().unwrap().to_owned();
            let workspace = journal.workspace(track).await?;
            assert_eq!(std::fs::read_to_string(workspace.path.join("single.txt"))?, "DUMMY_ONE_JOB\n");
            assert!(!workspace.path.join("one.txt").exists());
            assert_eq!(journal.plans().await?.into_iter().find(|plan| plan.id == untouched.id).unwrap().state, State::Draft);
            std::fs::write(output.join("result.json"), serde_json::to_vec_pretty(&json!({"injectedPlannerTimeout":true,"reflexStub":true,"originalRequestOnly":true,"workerCount":1,"paidTurns":1,"snapshot":final_state}))?)?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }.await;
        runtime.shutdown().await;
        preparation.shutdown().await;
        editor.stop().await;
        result?;
        assert_eq!(pool.snapshot().active, 0);
        Ok(())
    }
    #[tokio::test]
    #[ignore = "two paid dummy worker turns; requires saved auth and INTERCAT_M3_START_OUTPUT"]
    async fn real_reviewed_plan_inherits_files_before_the_second_worker_starts() -> Result<()> {
        real_plan(true, ChildWorkspace::New).await
    }
    #[tokio::test]
    #[ignore = "two paid dummy worker turns with an existing workspace"]
    async fn real_reviewed_plan_applies_to_an_existing_worker_and_preserves_its_files() -> Result<()>
    {
        real_plan(true, ChildWorkspace::Existing).await
    }
    #[tokio::test]
    #[ignore = "three paid dummy worker turns, including an active inheritance boundary"]
    async fn real_inheritance_waits_for_the_active_worker_and_its_checkpoint() -> Result<()> {
        real_plan(true, ChildWorkspace::Running).await
    }
    #[tokio::test]
    #[ignore = "two paid dummy worker turns with separate project folders and frozen dependency references"]
    async fn real_cross_folder_plan_uses_the_saved_parent_and_keeps_own_files_separate()
    -> Result<()> {
        real_plan(false, ChildWorkspace::OtherFolder).await
    }
    #[tokio::test]
    #[ignore = "two paid dummy turns after moving and undoing a plan, then restoring taken-back words"]
    async fn real_moved_plan_keeps_taken_back_words_restorable_after_execution() -> Result<()> {
        real_plan(false, ChildWorkspace::Edited).await
    }
    #[tokio::test]
    #[ignore = "two paid dummy worker turns; starts one item while keeping the plan held"]
    async fn real_start_without_keeps_the_parent_failed_and_other_items_held() -> Result<()> {
        real_plan(false, ChildWorkspace::StartWithout).await
    }
    #[tokio::test]
    #[ignore = "four paid dummy turns including an ordinary task and its explicit dependency retry"]
    async fn real_external_clip_retry_completes_before_the_dependent_plan_item() -> Result<()> {
        real_plan(false, ChildWorkspace::ExternalRetry).await
    }
    #[tokio::test]
    #[ignore = "two paid dummy turns; unlocks, moves, undoes and redoes while the first item runs"]
    async fn real_unlock_move_preserves_the_running_item_and_executes_the_new_destination()
    -> Result<()> {
        real_plan(false, ChildWorkspace::UnlockMove).await
    }
    #[tokio::test]
    #[ignore = "two paid dummy turns, started by visible automatic observations after an unseen interval"]
    async fn real_automatic_plan_waits_for_visibility_and_dependency_success() -> Result<()> {
        real_plan(true, ChildWorkspace::Automatic).await
    }
    #[tokio::test]
    #[ignore = "three paid dummy turns: read-only Planner, conflict review, and two workers"]
    async fn real_generated_plan_reviews_conflict_and_runs_two_items() -> Result<()> {
        real_plan(true, ChildWorkspace::Generated).await
    }
    #[tokio::test]
    #[ignore = "four paid dummy turns: plan, Reply correction, then two workers"]
    async fn real_plan_reply_preserves_ids_and_removed_work_through_execution() -> Result<()> {
        real_plan(true, ChildWorkspace::GeneratedReply).await
    }
    #[tokio::test]
    #[ignore = "three paid dummy turns: Planner assumptions followed by two real workers"]
    async fn real_planner_assumptions_follow_worker_execution() -> Result<()> {
        real_plan(true, ChildWorkspace::GeneratedAssumptions).await
    }
    async fn generated_fixture(
        reply_to_conflict: bool,
        with_assumptions: bool,
    ) -> Result<(tempfile::TempDir, Journal, Plan)> {
        let request = "Plan exactly three small disposable jobs in this order. The local IDs are one, two, and three. Job one uses new:1 and creates one.txt containing exactly DUMMY_one and index.txt containing exactly FIRST. Job two uses new:2, depends on one with inherit=true, verifies that inherited one.txt contains DUMMY_one, creates two.txt containing exactly DUMMY_two, and changes index.txt to exactly SECOND. Job three uses new:1 and creates three.txt containing exactly DUMMY_three; I will cut this job before starting. Do not combine these three jobs or add other jobs. Record that jobs one and two both change index.txt as a conflict between new:1 and new:2 for my review. Each worker must only do its own job. Workers must use file read and edit tools only; do not run shell commands, bash, terminal tools, or subprocesses. Workers must not use network, create commits, spawn agents, or ask questions, and must finish with Result: done when its files contain the specified text. Plan only; do not implement files.";
        let request = if with_assumptions {
            format!(
                "{request} The line-ending convention for generated files has not been chosen by the user. Choose LF or CRLF yourself, record the choice as a planning assumption with what equal to Line endings and default equal to LF or CRLF, and instruct every item to use that convention. Terminate each generated file with one newline using that convention. Do not ask me to decide."
            )
        } else {
            request.into()
        };
        let QueuedFixture {
            root,
            journal,
            plan,
            ..
        } = queued_fixture(None, false, &request).await?;
        let output = std::path::PathBuf::from(std::env::var("INTERCAT_M3_START_OUTPUT")?);
        std::fs::create_dir_all(&output)?;
        let config = crate::runtime::Config {
            source: std::env::var("INTERCAT_MUSE_BINARY")?.into(),
            binary: std::env::var("INTERCAT_MUSE_BINARY")?.into(),
            credentials: std::env::var("INTERCAT_MUSE_AUTH")?.into(),
            path: std::env::var("PATH")?,
        };
        let rpc = root.path().join("planner-rpc");
        std::fs::create_dir(&rpc)?;
        journal
            .begin_plan(plan.id, plan.generation.id, plan.rev)
            .await?;
        let gate = std::sync::Arc::new(crate::dispatch_control::Gate::new(false));
        let guard = journal
            .plan_guard(&plan, gate)
            .await?
            .ok_or("Missing Planner guard")?;
        let (_stop, stopping) = tokio::sync::watch::channel(false);
        let (draft, based_on) = crate::planner::run(
            crate::host_memory::Pool::new(),
            &journal,
            &config,
            root.path(),
            &rpc,
            &plan,
            guard,
            stopping,
        )
        .await?;
        std::fs::write(
            output.join("generated-draft.json"),
            serde_json::to_vec_pretty(&json!({"draft":draft,"basedOn":based_on,"paidTurns":1}))?,
        )?;
        if with_assumptions {
            assert!(
                draft
                    .assumptions
                    .iter()
                    .any(|value| value.what == "Line endings"
                        && matches!(value.default.as_str(), "LF" | "CRLF"))
            );
        }
        assert_eq!(draft.items.len(), 3);
        for (item, id) in draft.items.iter().zip(["one", "two", "three"]) {
            assert_eq!(item.id, id);
            assert!(item.instruction.contains(&format!("{id}.txt")));
        }
        assert!(draft.conflicts.iter().any(|finding| {
            (finding.track_a == "new:1" && finding.track_b == "new:2"
                || finding.track_a == "new:2" && finding.track_b == "new:1")
                && finding.what.contains("index.txt")
        }));
        journal
            .generated_plan(
                plan.id,
                plan.generation.id,
                plan.rev,
                Generated::Draft(draft),
                based_on,
            )
            .await?;
        let mut ready = journal.plans().await?.pop().unwrap();
        assert_eq!(ready.generation.state, crate::plans::GenerationState::Ready);
        assert_eq!(ready.items[1].after.len(), 1);
        assert!(ready.items[1].after[0].inherit);
        assert_eq!(
            ready.items[1].after[0].id,
            ready.items[0].plan_item_id.to_string()
        );
        let mut snapshot = journal.snapshot().await?;
        assert!(snapshot["workers"].as_array().unwrap().is_empty());
        assert_eq!(snapshot["proposal"]["kind"], "plan-conflict");
        assert!(snapshot["proposal"]["options"][1]["disabledReason"].is_string());
        if reply_to_conflict {
            ready = journal
                .plan_command(command(
                    &ready,
                    Action::Cut {
                        item_id: ready.items[2].plan_item_id,
                    },
                ))
                .await?;
            let original = ready.clone();
            let proposal = journal.snapshot().await?["proposal"].clone();
            let reply=crate::plan_feedback::Reply{op_id:Uuid::now_v7(),plan_id:ready.id,revision:ready.rev,question_id:proposal["id"].as_str().unwrap().parse()?,question:proposal["text"].as_str().unwrap().into(),text:"Keep the existing IDs, tracks, order and inheritance. In the second item only, change the value written to index.txt from SECOND to SECOND_REVISED. Preserve DUMMY_two in two.txt. Keep the third item removed. Use file read and edit tools only; never run shell commands, use the network, ask questions or create commits.".into()};
            let receipt = journal.reply_to_plan(reply.clone()).await?;
            assert_eq!(journal.reply_to_plan(reply).await?, receipt);
            let queued = journal.plans().await?.pop().unwrap();
            journal
                .begin_plan(queued.id, queued.generation.id, queued.rev)
                .await?;
            let guard = journal
                .plan_guard(
                    &queued,
                    std::sync::Arc::new(crate::dispatch_control::Gate::new(false)),
                )
                .await?
                .ok_or("Missing revised Planner guard")?;
            let (_stop, stopping) = tokio::sync::watch::channel(false);
            let (draft, based_on) = crate::planner::run(
                crate::host_memory::Pool::new(),
                &journal,
                &config,
                root.path(),
                &rpc,
                &queued,
                guard,
                stopping,
            )
            .await?;
            std::fs::write(
                output.join("revised-draft.json"),
                serde_json::to_vec_pretty(
                    &json!({"draft":draft,"basedOn":based_on,"receipt":receipt,"paidTurns":2}),
                )?,
            )?;
            journal
                .generated_plan(
                    queued.id,
                    queued.generation.id,
                    queued.rev,
                    Generated::Draft(draft),
                    based_on,
                )
                .await?;
            ready = journal.plans().await?.pop().unwrap();
            assert_eq!(ready.generation.state, crate::plans::GenerationState::Ready);
            assert_eq!(ready.items.len(), 3);
            for (before, after) in original.items.iter().zip(&ready.items) {
                assert_eq!(before.plan_item_id, after.plan_item_id);
            }
            assert_eq!(ready.items[2].state, ItemState::Cut);
            assert!(ready.items[1].instruction.contains("SECOND_REVISED"));
            snapshot = journal.snapshot().await?;
        }
        if !snapshot["proposal"].is_null() {
            let answer = crate::proposals::Command {
                op_id: Uuid::now_v7(),
                proposal_id: snapshot["proposal"]["id"].as_str().unwrap().parse()?,
                action: crate::proposals::Action::KeepPlan,
            };
            let receipt = journal.answer_proposal(answer.clone()).await?;
            assert_eq!(journal.answer_proposal(answer).await?, receipt);
            std::fs::write(
                output.join("conflict-review.json"),
                serde_json::to_vec_pretty(
                    &json!({"before":snapshot,"receipt":receipt,"after":journal.snapshot().await?}),
                )?,
            )?;
        }
        assert!(journal.snapshot().await?["proposal"].is_null());
        assert_eq!(
            std::fs::read_dir(root.path().join("planner-hosts"))?.count(),
            0
        );
        Ok((root, journal, ready))
    }
    enum ChildWorkspace {
        New,
        Generated,
        GeneratedReply,
        GeneratedAssumptions,
        Automatic,
        RetryItem,
        StartWithout,
        ExternalRetry,
        UnlockMove,
        SpokenGo,
        Edited,
        Existing,
        Running,
        OtherFolder,
    }
    async fn real_plan(inherit: bool, child: ChildWorkspace) -> Result<()> {
        let automatic = matches!(child, ChildWorkspace::Automatic);
        let existing = matches!(child, ChildWorkspace::Existing | ChildWorkspace::Running);
        let active = matches!(child, ChildWorkspace::Running);
        let cross_folder = matches!(child, ChildWorkspace::OtherFolder);
        let spoken = matches!(child, ChildWorkspace::SpokenGo);
        let retry_item = matches!(child, ChildWorkspace::RetryItem);
        let start_without = matches!(child, ChildWorkspace::StartWithout);
        let external_retry = matches!(child, ChildWorkspace::ExternalRetry);
        let unlock_move = matches!(child, ChildWorkspace::UnlockMove);
        let marker = format!("DUMMY_REFERENCE_{}", Uuid::now_v7());
        let reply_to_conflict = matches!(child, ChildWorkspace::GeneratedReply);
        let with_assumptions = matches!(child, ChildWorkspace::GeneratedAssumptions);
        let generated = matches!(
            child,
            ChildWorkspace::Generated
                | ChildWorkspace::GeneratedReply
                | ChildWorkspace::GeneratedAssumptions
        );
        let (root, journal, mut plan) = if generated {
            generated_fixture(reply_to_conflict, with_assumptions).await?
        } else if cross_folder {
            fixture_layout(true, Some(&marker), false).await?
        } else {
            fixture_layout(inherit, None, automatic).await?
        };
        let move_target = if unlock_move {
            let db = Connection::open(root.path().join("db"))?;
            crate::clock::register(&db)?;
            let a = Uuid::now_v7().to_string();
            let b = Uuid::now_v7().to_string();
            for (id, label) in [(&a, "A"), (&b, "B")] {
                db.execute(
                    "INSERT INTO tracks VALUES(?1,?2,'Dummy unlock target','planned')",
                    params![id, label],
                )?;
                crate::folders::bind(&db, id, &plan.items[0].folder_id)?;
            }
            plan.locked_track = Some(a.clone());
            for item in &mut plan.items {
                item.track_ref = a.clone();
            }
            db.execute(
                "UPDATE utterances SET target=?2 WHERE id=?1",
                params![
                    plan.source_id,
                    serde_json::to_string(&crate::decision::Target::Lock(a))?
                ],
            )?;
            let context = crate::plan_store::context(&db, plan.id)?;
            crate::plans::validate_edit(&mut plan, &context).map_err(|errors| errors.join("\n"))?;
            crate::plan_store::save(&db, &plan, "fixture.locked-move")?;
            Some(b)
        } else {
            None
        };
        if retry_item || start_without {
            plan.items[0].instruction = "This is a disposable two-stage test. Read whether one.txt exists. If it does not exist, create one.txt containing exactly DUMMY_one and then end this turn immediately with Result: partial — a second test step is deliberately left for the explicit retry. Do not create retry-complete.txt in that first turn. If one.txt already exists, verify its content is DUMMY_one, preserve it, create retry-complete.txt containing exactly RETRY_DONE, and report Result: done — both dummy test steps are complete. Do not edit other files, use the network, make commits, or ask questions.".into();
            plan.items[0].done_when =
                "one.txt contains DUMMY_one and retry-complete.txt contains RETRY_DONE.".into();
            let db = Connection::open(root.path().join("db"))?;
            crate::clock::register(&db)?;
            crate::plan_store::save(&db, &plan, "fixture.retry-item")?;
        }
        if cross_folder {
            assert_ne!(plan.items[0].folder_id, plan.items[1].folder_id);
            assert!(!plan.items[1].after[0].inherit);
            assert!(!plan.notices.is_empty());
        }
        let edited = matches!(child, ChildWorkspace::Edited);
        let taken_back = if edited {
            let id = Uuid::now_v7();
            journal
                .stage_utterance(
                    id,
                    "Dummy words kept while a moved plan runs".into(),
                    crate::decision::Target::New,
                    None,
                    vec![],
                )
                .await?;
            journal.cut_input(Uuid::now_v7(), id.to_string()).await?;
            let moved = journal
                .plan_command(command(
                    &plan,
                    Action::Move {
                        item_id: plan.items[2].plan_item_id,
                        direction: crate::plan_store::Direction::Earlier,
                    },
                ))
                .await?;
            assert!(moved.items[2].order < moved.items[0].order);
            let snapshot = journal.snapshot().await?;
            journal
                .edit_history(crate::timeline_history::Request {
                    op_id: Uuid::now_v7(),
                    action: crate::timeline_history::Action::Undo {
                        entry: snapshot["editHistory"]["undo"]["entryId"]
                            .as_str()
                            .unwrap()
                            .into(),
                    },
                })
                .await?;
            plan = journal.plans().await?.pop().unwrap();
            assert!(plan.items[0].order < plan.items[2].order);
            Some(id)
        } else {
            None
        };
        let original_folders: Vec<_> = if cross_folder {
            let project: crate::project::Project =
                serde_json::from_value(journal.snapshot().await?["project"].clone())?;
            project
                .folders
                .iter()
                .map(|folder| {
                    Ok((
                        folder.root.clone(),
                        std::fs::read(folder.root.join(".git/index"))?,
                        folder.base.clone(),
                    ))
                })
                .collect::<Result<_>>()?
        } else {
            vec![]
        };
        let output = std::path::PathBuf::from(std::env::var("INTERCAT_M3_START_OUTPUT")?);
        std::fs::create_dir_all(&output)?;
        let config = crate::runtime::Config {
            source: std::env::var("INTERCAT_MUSE_BINARY")?.into(),
            binary: std::env::var("INTERCAT_MUSE_BINARY")?.into(),
            credentials: std::env::var("INTERCAT_MUSE_AUTH")?.into(),
            path: std::env::var("PATH")?,
        };
        journal.set_worker_config(Uuid::now_v7(), config).await?;
        let rpc = root.path().join("rpc");
        std::fs::create_dir(&rpc)?;
        let workspaces = root.path().join("workspaces");
        let mut preparation = Some(crate::preparation::Preparation::start(
            &workspaces,
            journal.clone(),
        )?);
        let workspaces = workspaces.canonicalize()?;
        let editor = ready_editor(root.path()).await?;
        let gate = std::sync::Arc::new(crate::dispatch_control::Gate::new(false));
        let runtime = crate::runtime::Runtime::start(
            crate::host_memory::Pool::new(),
            root.path().into(),
            rpc.clone(),
            journal.clone(),
            editor.clone(),
            std::sync::Arc::new(crate::scheduler::Capacity::new(2)),
            gate.clone(),
        );
        let project_id = journal.snapshot().await?["projectSecrets"]["projectId"]
            .as_str()
            .unwrap()
            .to_owned();
        let key = Key::new([7; 32]);
        journal
            .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
            .await?;
        let catalog = journal
            .prepare_project_secrets(crate::project_secrets::Prepare {
                op_id: Uuid::now_v7(),
                project_id: project_id.clone(),
                revision: 0,
                fingerprints: vec![],
                key_check: key
                    .check()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            })
            .await?;
        journal.confirm_fingerprints(catalog.current).await?;
        runtime
            .service
            .activate_project_secrets(crate::project_secrets::Load {
                project_id,
                revision: 0,
                entries: vec![],
            })
            .await?;
        let planning = crate::planning::Service::start(
            crate::host_memory::Pool::new(),
            journal.clone(),
            root.path().into(),
            rpc,
            runtime.service.assistant_pool(),
            gate,
        );
        let mut reference_root = None;
        let result = async {
            if existing {
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while journal.snapshot().await?["session"]["bound"] != true {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                    Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
                })
                .await??;
                preparation.take().unwrap().shutdown().await;
            }
            journal.advance_plans().await?;
            assert!(
                journal.snapshot().await?["workers"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            let cut = if plan.items[2].state == ItemState::Cut {plan.clone()} else {
                journal.plan_command(command(&plan,Action::Cut{item_id:plan.items[2].plan_item_id})).await?
            };
            if !automatic { journal.plan_command(command(&cut, Action::Review)).await?; }
            let started = if automatic {
                let unseen_at=std::time::Instant::now();
                tokio::time::sleep(std::time::Duration::from_millis(5_500)).await;
                let unseen=journal.plans().await?.into_iter().find(|plan|plan.id==cut.id).unwrap();
                assert_eq!(unseen.state,State::Draft);
                assert_eq!(unseen.seen_revision,None);
                assert!(unseen.items.iter().all(|item|item.utterance_id.is_none()));
                assert!(journal.snapshot().await?["workers"].as_array().unwrap().is_empty());
                let unseen_ms=unseen_at.elapsed().as_millis();
                let owner=Uuid::now_v7();let seen_at=std::time::Instant::now();let mut sequence=0;
                let (started,observations)=tokio::time::timeout(std::time::Duration::from_secs(15),async {
                    let mut observations=Vec::new();
                    loop {
                        sequence+=1;
                        let update=journal.observe_plans(crate::plan_start::Observation{owner,sequence,idle:true,plans:vec![crate::plan_start::Seen{plan_id:cut.id,revision:cut.rev}]}).await?;
                        observations.push(json!({"elapsedMs":seen_at.elapsed().as_millis(),"countdowns":update.countdowns,"started":update.started.len()}));
                        if let Some(started)=update.started.into_iter().next(){break Ok::<_,Box<dyn std::error::Error+Send+Sync>>((started,observations));}
                        assert!(journal.snapshot().await?["workers"].as_array().unwrap().is_empty());
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    }
                }).await??;
                assert!(seen_at.elapsed().as_millis()>=5_000);
                assert_eq!(started.state,State::Running);
                assert_eq!(started.start_mode,crate::plan_start::Mode::AfterDelay);
                let db=Connection::open(root.path().join("db"))?;
                assert_eq!(db.query_row("SELECT count(*) FROM events WHERE kind='plan.started-after-delay'",[],|row|row.get::<_,i64>(0))?,1);
                assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind='plan.command' AND json_extract(payload,'$.kind')='start'",[],|row|row.get::<_,i64>(0))?,0);
                std::fs::write(output.join("automatic-start.json"),serde_json::to_vec_pretty(&json!({"unseenMs":unseen_ms,"observedUntilStartMs":seen_at.elapsed().as_millis(),"observations":observations,"plan":started}))?)?;
                started
            } else if spoken {
                let go = Uuid::now_v7();
                journal.stage_utterance(go, "Go ahead with this reviewed dummy plan.".into(), crate::decision::Target::None, None, vec![]).await?;
                let input = journal.claim_input().await?.unwrap();
                assert_eq!(input.id, go.to_string());
                journal.apply_input(input.id, input.revision, crate::decision::Outcome::Classified(crate::decision::Decision {
                    kind: crate::decision::Kind::Command, track_id: None, boundary: None, command: Some("start-waiting".into()), confidence: 1., inference_ms: 0., timed_out: false, scores: json!({}),
                })).await?;
                let word = journal.read_utterance(go.to_string()).await?;
                assert_eq!(word["state"], "landed");
                assert_eq!(word["startWaiting"]["started"]["planId"], cut.id.to_string());
                let started = journal.plans().await?.into_iter().find(|plan| plan.id == cut.id).unwrap();
                assert_eq!(started.state, State::Running);
                assert!(!started.execute_allowed);
                std::fs::write(output.join("spoken-go.json"), serde_json::to_vec_pretty(&word)?)?;
                started
            } else { journal.plan_command(command(&cut, Action::Start)).await? };
            if existing {
                let snapshot = journal.snapshot().await?;
                let track = &started.items[1].track_ref;
                let label = snapshot["tracks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|value| value["id"] == *track)
                    .unwrap()["label"]
                    .as_str()
                    .unwrap();
                let project: crate::project::Project =
                    serde_json::from_value(snapshot["project"].clone())?;
                let source = project.only_folder()?.source().await?;
                let mut workspace = crate::preparation::Workspace {
                    denied_roots: Vec::new(),
                    track_id: track.clone(),
                    path: workspaces
                        .join(snapshot["session"]["id"].as_str().unwrap())
                        .join(track),
                    reference: format!(
                        "refs/intercat/{}/tracks/{label}",
                        snapshot["session"]["slug"].as_str().unwrap()
                    ),
                    base: source.head.to_string(),
                    state: "preparing".into(),
                    error: None,
                    attachment_roots: vec![],
                };
                journal.workspace_result(workspace.clone()).await?;
                crate::worktree::prepare(
                    &source,
                    &workspace.path,
                    &workspace.reference,
                    Uuid::new_v5(&track.parse()?, b"worktree.prepare"),
                    &journal,
                )
                .await?;
                std::fs::write(workspace.path.join("keep.txt"), "KEEP_EXISTING_WORK\n")?;
                if active {
                    std::fs::write(workspace.path.join("hold.py"), "from pathlib import Path\nimport time\nPath('active.txt').write_text('ACTIVE\\n')\nend=time.monotonic()+180\nwhile not Path('release.txt').exists():\n if time.monotonic()>end: raise SystemExit('dummy wait timed out')\n time.sleep(.02)\nPath('working.txt').write_text('FINISHED\\n')\nprint('dummy work finished')\n")?;
                }
                workspace.state = "ready".into();
                journal.workspace_result(workspace.clone()).await?;
                if active {
                    let id=Uuid::now_v7();
                    journal.stage_utterance(id,"This is a disposable local test. Run exactly `python3 hold.py` once with bash and wait for its result. Do not edit files, run other commands or use the network. The test harness will release its finite wait.".into(),crate::decision::Target::Lock(track.clone()),None,vec![]).await?;
                    let input=journal.claim_input().await?.ok_or("The dummy active task was not queued.")?;
                    assert_eq!(input.id,id.to_string());
                    journal.apply_input(input.id,input.revision,crate::decision::Outcome::Classified(crate::decision::Decision{kind:crate::decision::Kind::Extend,track_id:Some(track.clone()),boundary:Some("now".into()),command:None,confidence:1.,inference_ms:0.,timed_out:false,scores:json!({})})).await?;
                    tokio::time::timeout(std::time::Duration::from_secs(60),async {
                        while !workspace.path.join("active.txt").exists() {
                            std::fs::write(output.join("latest.json"),serde_json::to_vec_pretty(&journal.snapshot().await?)?)?;
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
                    }).await??;
                }
                preparation = Some(crate::preparation::Preparation::start(
                    &workspaces,
                    journal.clone(),
                )?);
            }
            let mut released = !active;
            let mut cross_held = false;
            let mut cross_resumed = false;
            let mut retry_input = None;
            let mut manually_started = false;
            let mut external_input = None;
            let mut external_attempt = None;
            let mut external_clip = None;
            let mut external_resumed = false;
            let mut move_verified = false;
            let mut automatic_dependency = None;
            tokio::time::timeout(std::time::Duration::from_secs(180), async {
                loop {
                    let snapshot = journal.snapshot().await?;
                    std::fs::write(
                        output.join("latest.json"),
                        serde_json::to_vec_pretty(&snapshot)?,
                    )?;
                    if let Some(worker) =
                        snapshot["workers"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|worker| {
                                matches!(worker["phase"].as_str(), Some("held" | "failed")) && worker["detail"].as_str().is_some()
                            })
                    {
                        return Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                            format!("Dummy worker unavailable: {}", worker["detail"].as_str().unwrap())
                                .into(),
                        );
                    }
                    for report in snapshot["reports"].as_array().into_iter().flatten() {
                        let id = report["id"]
                            .as_str()
                            .ok_or("Missing report ID.")?
                            .to_owned();
                        let revision = report["revision"].as_i64().unwrap();
                        journal
                            .analyze_report(id, revision, crate::reports::Analysis::Clear)
                            .await?;
                    }
                    let plan = journal.plans().await?.pop().unwrap();
                    if automatic {
                        if plan.items[0].state != ItemState::Succeeded {
                            assert!(plan.items[1].utterance_id.is_none(), "A dependent input appeared before its prerequisite succeeded.");
                        } else if let Some(child_input)=plan.items[1].utterance_id && automatic_dependency.is_none() {
                            let parent_input=plan.items[0].utterance_id.unwrap();
                            let origin=journal.read_input_origin(parent_input).await?.ok_or("Missing automatic prerequisite origin")?;
                            assert!(origin.checkpoint.is_some());
                            automatic_dependency=Some(json!({"parentInput":parent_input,"childInput":child_input,"parentState":plan.items[0].state,"parentTurn":origin.identity.turn_id,"checkpoint":origin.checkpoint}));
                        }
                    }
                    if unlock_move && !move_verified && plan.items[0].state == ItemState::Running
                        && let Some(input)=plan.items[0].utterance_id
                        && let Some(origin)=journal.read_input_origin(input).await?
                        && origin.phase==crate::input_origin::Phase::Running {
                        assert!(plan.items[1].utterance_id.is_none());
                        let first=plan.items[0].clone();
                        let from=plan.locked_track.clone().unwrap();
                        let target=move_target.clone().unwrap();
                        let request=command(&plan,Action::UnlockAndMove {item_id:plan.items[1].plan_item_id,track_id:target.clone()});
                        let moved=journal.plan_command(request.clone()).await?;
                        assert_eq!(moved.items[0],first);
                        assert_eq!(journal.plan_command(request.clone()).await?.rev,moved.rev);
                        let history=journal.snapshot().await?["editHistory"]["undo"]["entryId"].as_str().unwrap().to_owned();
                        journal.edit_history(crate::timeline_history::Request {op_id:Uuid::now_v7(),action:crate::timeline_history::Action::Undo {entry:history.clone()}}).await?;
                        let restored=journal.plans().await?.pop().unwrap();
                        assert_eq!(restored.items[0],first);assert_eq!(restored.locked_track,Some(from.clone()));
                        assert_eq!(restored.items[1].track_ref,from);
                        journal.edit_history(crate::timeline_history::Request {op_id:Uuid::now_v7(),action:crate::timeline_history::Action::Redo {entry:history}}).await?;
                        let redone=journal.plans().await?.pop().unwrap();
                        assert_eq!(redone.items[0],first);assert_eq!(redone.items[1].track_ref,target);
                        assert!(redone.locked_track.is_none());
                        assert_eq!(journal.read_input_origin(input).await?.unwrap().identity,origin.identity);
                        std::fs::write(output.join("unlock-move-command.json"),serde_json::to_vec_pretty(&request)?)?;
                        std::fs::write(output.join("while-first-running.json"),serde_json::to_vec_pretty(&snapshot)?)?;
                        move_verified=true;
                        continue;
                    }
                    if start_without && !manually_started && plan.items[0].state == ItemState::Failed {
                        assert!(plan.items[1].utterance_id.is_none());
                        let held = journal.plan_command(command(&plan, Action::Restore { item_id: plan.items[2].plan_item_id })).await?;
                        assert_eq!(held.state, State::Held);
                        let request = command(&held, Action::StartWithout { item_id: held.items[1].plan_item_id, dependency_id: held.items[0].plan_item_id.to_string() });
                        let queued = journal.plan_command(request.clone()).await?;
                        assert_eq!(journal.plan_command(request.clone()).await?.rev, queued.rev);
                        assert_eq!(queued.state, State::Held);
                        assert!(queued.items[1].start_requested);
                        std::fs::write(output.join("start-without-command.json"), serde_json::to_vec_pretty(&request)?)?;
                        std::fs::write(output.join("before-start-without.json"), serde_json::to_vec_pretty(&snapshot)?)?;
                        manually_started = true;
                        continue;
                    }
                    if retry_item && retry_input.is_none() && plan.items[0].state == ItemState::Failed {
                        assert!(plan.items[1].utterance_id.is_none());
                        let origin = journal.read_input_origin(plan.items[0].utterance_id.unwrap()).await?.unwrap();
                        let db = Connection::open(root.path().join("db"))?;
                        assert_eq!(crate::report_result::final_report(&db, &origin.identity.session_id, &origin.identity.turn_id.to_string())?.unwrap()["kind"], "partial");
                        let request = command(&plan, Action::RetryItem { item_id: plan.items[0].plan_item_id });
                        let queued = journal.plan_command(request.clone()).await?;
                        assert_eq!(journal.plan_command(request.clone()).await?.rev, queued.rev);
                        retry_input = Some(Uuid::new_v5(&request.op_id, b"retry-plan-item"));
                        std::fs::write(output.join("retry-command.json"), serde_json::to_vec_pretty(&request)?)?;
                        std::fs::write(output.join("before-retry.json"), serde_json::to_vec_pretty(&snapshot)?)?;
                        continue;
                    }
                    if (cross_folder || external_retry) && !cross_held && plan.items[0].state == ItemState::Running {
                        journal.plan_command(command(&plan, Action::Hold)).await?;
                        cross_held = true;
                    }
                    if cross_folder && cross_held && !cross_resumed && plan.items[0].state == ItemState::Succeeded {
                        assert!(plan.items[1].utterance_id.is_none(), "The held successor was queued before its explicit start.");
                        let parent = journal.workspace(plan.items[0].track_ref.clone()).await?;
                        assert_eq!(std::fs::read_to_string(parent.path.join("one.txt"))?.trim(),marker);
                        std::fs::write(parent.path.join("one.txt"), "UNSAVED_PARENT_CHANGE\n")?;
                        std::fs::write(output.join("before-child-start.json"),serde_json::to_vec_pretty(&snapshot)?)?;
                        journal.plan_command(command(&plan,Action::Review)).await?;
                        journal.plan_command(command(&plan,Action::Start)).await?;
                        cross_resumed = true;
                    }
                    if external_retry && cross_held && external_input.is_none() && plan.items[0].state == ItemState::Succeeded {
                        assert!(plan.items[1].utterance_id.is_none());
                        let id=Uuid::now_v7();
                        journal.stage_utterance(id,"This is a disposable ordinary task outside the plan. Read whether ext.txt exists. If it does not exist, create ext.txt containing exactly EXT_READY and end this turn with Result: partial — the second dummy step remains. Do not create ext-done.txt yet. If ext.txt exists, keep its contents, create ext-done.txt containing exactly EXT_DONE, and report Result: done — the ordinary dummy task is finished. Do not edit one.txt, use the network, make commits or ask questions.".into(),crate::decision::Target::Lock(plan.items[0].track_ref.clone()),None,vec![]).await?;
                        let input=journal.claim_input().await?.ok_or("The ordinary external task was not queued")?;
                        assert_eq!(input.id,id.to_string());
                        journal.apply_input(input.id,input.revision,crate::decision::Outcome::Classified(crate::decision::Decision{kind:crate::decision::Kind::Extend,track_id:Some(plan.items[0].track_ref.clone()),boundary:Some("after-turn".into()),command:None,confidence:1.,inference_ms:0.,timed_out:false,scores:json!({})})).await?;
                        external_input=Some(id);
                    }
                    if external_retry && external_attempt.is_none() && let Some(input)=external_input
                        && let Some(origin)=journal.read_input_origin(input).await?
                        && matches!(origin.phase,crate::input_origin::Phase::Finished|crate::input_origin::Phase::MovedOn) && origin.checkpoint.is_some() {
                        let db=Connection::open(root.path().join("db"))?;
                        assert!(crate::plan_execution::owner(&db,&input.to_string())?.is_none());
                        assert_eq!(crate::report_result::final_report(&db,&origin.identity.session_id,&origin.identity.turn_id.to_string())?.unwrap()["kind"],"partial");
                        let clip=crate::dependency_retry::current_clip(&db,&origin)?.ok_or("Missing ordinary task clip")?;
                        let owner=Uuid::now_v7();let token=Uuid::now_v7();
                        let edit=journal.edit_plan(crate::plan_rewrite::Request{owner,action:crate::plan_rewrite::Action::Begin{plan_id:plan.id,item_id:plan.items[1].plan_item_id,token,revision:plan.rev}}).await?.unwrap();
                        let edited=journal.edit_plan(crate::plan_rewrite::Request{owner,action:crate::plan_rewrite::Action::Dependencies{plan_id:edit.id,token,revision:edit.rev,op_id:Uuid::now_v7(),after:vec![crate::plans::Dependency{id:clip.clone(),inherit:false}]}}).await?.unwrap();
                        let request=command(&edited,Action::RetryDependency{item_id:edited.items[1].plan_item_id,dependency_id:clip.clone(),clip_id:clip.clone()});
                        let queued=journal.plan_command(request.clone()).await?;
                        assert_eq!(journal.plan_command(request.clone()).await?.rev,queued.rev);
                        external_attempt=Some(Uuid::new_v5(&request.op_id,b"retry-dependency"));external_clip=Some(clip);
                        std::fs::write(output.join("external-retry-command.json"),serde_json::to_vec_pretty(&request)?)?;
                        continue;
                    }
                    if external_retry && !external_resumed && let Some(input)=external_attempt
                        && let Some(origin)=journal.read_input_origin(input).await?
                        && matches!(origin.phase,crate::input_origin::Phase::Finished|crate::input_origin::Phase::MovedOn) && origin.checkpoint.is_some() {
                        let db=Connection::open(root.path().join("db"))?;
                        assert_eq!(crate::report_result::final_report(&db,&origin.identity.session_id,&origin.identity.turn_id.to_string())?.unwrap()["kind"],"done");
                        assert_eq!(crate::plan_dependencies::read(&db,std::slice::from_ref(&plan),external_clip.as_deref().unwrap())?.unwrap().state,ItemState::Succeeded);
                        assert!(plan.items[1].utterance_id.is_none());
                        journal.plan_command(command(&plan,Action::Review)).await?;
                        journal.plan_command(command(&plan,Action::Start)).await?;
                        external_resumed=true;
                    }
                    if !released && plan.items[0].state==ItemState::Succeeded && plan.items[1].utterance_id.is_some() {
                        let workspace=journal.workspace(plan.items[1].track_ref.clone()).await?;
                        let worker=snapshot["workers"].as_array().unwrap().iter().find(|worker|worker["trackId"]==plan.items[1].track_ref).unwrap();
                        assert!(worker["activeTurn"].is_string(),"The dummy B turn ended before the boundary was checked.");
                        assert!(!workspace.path.join("one.txt").exists(),"Inheritance changed files while B was active.");
                        assert!(!workspace.path.join("working.txt").exists());
                        let input=plan.items[1].utterance_id.unwrap();
                        assert!(journal.operation(Uuid::new_v5(&input,b"apply-inherited-files")).await?.is_none());
                        std::fs::write(output.join("while-active.json"),serde_json::to_vec_pretty(&snapshot)?)?;
                        std::fs::write(workspace.path.join("release.txt"),"release dummy work\n")?;
                        released=true;
                    }
                    if generated && snapshot["workers"].as_array().unwrap().iter().any(|worker| {
                        worker["approvals"].as_array().is_some_and(|values| !values.is_empty())
                            || worker["prompts"].as_array().is_some_and(|values| !values.is_empty())
                    }) {
                        return Err::<(), Box<dyn std::error::Error + Send + Sync>>("The dummy generated plan requested an unexpected approval or input.".into());
                    }
                    if plan
                        .items
                        .iter()
                        .enumerate().any(|(index,item)| item.state == ItemState::Failed
                            && !(index == 0 && (start_without || retry_item && retry_input.is_some() && item.utterance_id != retry_input)))
                    {
                        return Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                            "A dummy plan item failed.".into(),
                        );
                    }
                    if plan.state == State::Done || start_without && plan.items[1].state == ItemState::Succeeded {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
            })
            .await??;
            assert!(released);
            if automatic { std::fs::write(output.join("automatic-dependency.json"),serde_json::to_vec_pretty(&automatic_dependency.ok_or("Automatic dependency was never released")?)?)?; }
            if cross_folder { assert!(cross_held && cross_resumed); }
            if let Some(id)=taken_back {
                let word=journal.read_utterance(id.to_string()).await?;
                assert_eq!(word["state"],"cut");
                journal.edit_history(crate::timeline_history::Request {op_id:Uuid::now_v7(),action:crate::timeline_history::Action::Restore {input:id.to_string(),revision:word["revision"].as_i64().unwrap()}}).await?;
                assert_eq!(journal.read_utterance(id.to_string()).await?["state"],"held");
            }
            let mut final_state = journal.snapshot().await?;
            if unlock_move {
                assert!(move_verified);
                let plan=journal.plans().await?.pop().unwrap();
                assert_eq!(plan.items[1].track_ref,move_target.unwrap());
                assert_ne!(plan.items[0].track_ref,plan.items[1].track_ref);
                assert!(plan.locked_track.is_none());
                final_state["unlockMoveVerification"]=json!({"paidTurns":2,"movedWhileParentRunning":true,"parentIdentityPreserved":true,"undoAndRedo":true,"sameOperationReplayed":true,"reflexStub":true});
            }
            if start_without {
                let plan = journal.plans().await?.pop().unwrap();
                assert!(manually_started);
                assert_eq!(plan.state, State::Held);
                assert_eq!(plan.items[0].state, ItemState::Failed);
                assert_eq!(plan.items[1].state, ItemState::Succeeded);
                assert_eq!(plan.items[2].state, ItemState::Planned);
                assert!(plan.items[2].utterance_id.is_none());
                assert!(!plan.items[1].start_requested);
                assert!(plan.items[1].after.is_empty());
                final_state["startWithoutVerification"] = json!({"paidTurns":2,"parentRemainsFailed":true,"otherItemsRemainHeld":true,"sameOperationReplayed":true,"reflexStub":true});
            }
            if external_retry {
                assert!(external_resumed);
                let plan=journal.plans().await?.pop().unwrap();
                let workspace=journal.workspace(plan.items[0].track_ref.clone()).await?;
                assert_eq!(std::fs::read_to_string(workspace.path.join("ext.txt"))?.trim(),"EXT_READY");
                assert_eq!(std::fs::read_to_string(workspace.path.join("ext-done.txt"))?.trim(),"EXT_DONE");
                let db=Connection::open(root.path().join("db"))?;
                assert_eq!(db.query_row("SELECT count(*) FROM dependency_retries",[],|r|r.get::<_,i64>(0))?,1);
                final_state["externalRetryVerification"]=json!({"paidTurns":4,"sourceInput":external_input,"retryInput":external_attempt,"sourceClip":external_clip,"sameOperationReplayed":true,"ordinaryTaskHasNoPlanItem":true,"dependentWaitedForRetry":true,"reflexStub":true});
            }
            if generated {
                final_state["generatedPlanVerification"] = json!({"paidTurns":if reply_to_conflict {4}else{3},"realPlanner":true,"conflictReviewed":true,"generatedItemsUnchanged":true,"reflexStub":true,"replyCorrection":reply_to_conflict});
            }
            if spoken { final_state["goAheadVerification"] = json!({"reviewedBeforeWords":true,"planOnlySource":true,"reflexStub":true,"paidTurns":2}); }
            if retry_item {
                let plan = journal.plans().await?.pop().unwrap();
                assert_eq!(plan.items[0].utterance_id, retry_input);
                let workspace = journal.workspace(plan.items[0].track_ref.clone()).await?;
                assert_eq!(std::fs::read_to_string(workspace.path.join("retry-complete.txt"))?.trim(), "RETRY_DONE");
                let db = Connection::open(root.path().join("db"))?;
                assert_eq!(db.query_row("SELECT count(*) FROM plan_inputs WHERE item=?1 AND role='continuation'", [plan.items[0].plan_item_id.to_string()], |row| row.get::<_,i64>(0))?, 1);
                final_state["retryVerification"] = json!({"paidTurns":3,"retryInput":retry_input,"oneContinuation":true,"sameItem":true,"dependentWaitedForRetry":true,"reflexStub":true});
            }
            if let Some(id)=taken_back {final_state["editVerification"]=json!({"movedAndUndoneBeforeStart":true,"takenBackRestoredAfterPlan":true,"restoredInput":id,"restoredState":"held","paidTurns":2});}
            assert_eq!(final_state["workers"].as_array().unwrap().len(), 2);
            for (index, name) in ["one", "two"].iter().enumerate() {
                let plan = journal.plans().await?.pop().unwrap();
                let workspace = journal
                    .workspace(plan.items[index].track_ref.clone())
                    .await?;
                assert_eq!(
                    std::fs::read_to_string(workspace.path.join(format!("{name}.txt")))?.trim(),
                    if cross_folder && index == 0 { "UNSAVED_PARENT_CHANGE".into() } else if cross_folder { marker.clone() } else { format!("DUMMY_{name}") }
                );
                assert!(!workspace.path.join("three.txt").exists());
                if generated {
                    assert_eq!(std::fs::read_to_string(workspace.path.join("index.txt"))?.trim(), if index == 0 { "FIRST" } else if reply_to_conflict {"SECOND_REVISED"} else { "SECOND" });
                }
                if cross_folder {
                    assert!(workspace.path.join(if index == 0 { "a-base.txt" } else { "b-base.txt" }).exists());
                    assert!(!workspace.path.join(if index == 0 { "b-base.txt" } else { "a-base.txt" }).exists());
                    if index == 1 {
                        assert!(!workspace.path.join("one.txt").exists());
                        let input = plan.items[1].utterance_id.unwrap();
                        let db = Connection::open(root.path().join("db"))?;
                        let recipe = crate::dependency_references::read(&db, &input.to_string(), &plan.items[1].track_ref)?.ok_or("Missing frozen reference")?;
                        let parent_input = plan.items[0].utterance_id.unwrap();
                        let origin = crate::input_origin::read(&db,parent_input)?.ok_or("Missing parent origin")?;
                        assert_eq!(recipe.sources.len(),1);
                        assert_eq!(Some(recipe.sources[0].checkpoint.clone()),origin.checkpoint);
                        assert_eq!(recipe.sources[0].number,1);
                        let mounted = root.path().canonicalize()?.join("dependency-references").join(&plan.items[1].track_ref).join(input.to_string()).join("mounted");
                        let file = mounted.join(recipe.sources[0].name()?).join("one.txt");
                        assert_eq!(std::fs::read_to_string(&file)?.trim(),marker);
                        assert!(std::fs::write(&file,"must stay read-only").is_err());
                        reference_root = Some(mounted.clone());
                        final_state["referenceVerification"] = json!({"input":input,"checkpoint":recipe.sources[0].checkpoint,"number":recipe.sources[0].number,"mount":mounted,"savedParentOnly":true,"primaryHasNoParentFile":true,"readonly":true});
                        assert!(crate::lineage_store::basis(&db,&plan.items[1].track_ref)?.inherits.is_empty());
                    }
                }
                if inherit && index == 1 {
                    assert_eq!(
                        std::fs::read_to_string(workspace.path.join("one.txt"))?.trim(),
                        "DUMMY_one"
                    );
                    if existing {
                        assert_eq!(
                            std::fs::read_to_string(workspace.path.join("keep.txt"))?,
                            "KEEP_EXISTING_WORK\n"
                        );
                        if active {
                            assert_eq!(std::fs::read_to_string(workspace.path.join("working.txt"))?,"FINISHED\n");
                        }
                    }
                }
                let text = final_state["utterances"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|word| word["id"] == plan.items[index].utterance_id.unwrap().to_string())
                    .unwrap()["text"]
                    .as_str()
                    .unwrap();
                assert!(!text.contains("Original request sentinel"));
                if cross_folder && index == 1 { assert!(!text.contains(&marker)); }
            }
            if with_assumptions {
                let plan=journal.plans().await?.into_iter().find(|current|current.id==plan.id).ok_or("The completed dummy plan is missing.")?;
                let ending=plan.assumptions.iter().find(|value|value.what=="Line endings").unwrap().default.as_str();
                for (index,item) in plan.items[..2].iter().enumerate() {
                    let workspace=journal.workspace(item.track_ref.clone()).await?;
                    for name in [if index==0 {"one.txt"} else {"two.txt"},"index.txt"] {
                        let bytes=std::fs::read(workspace.path.join(name))?;
                        assert!(bytes.ends_with(if ending=="CRLF" {b"\r\n"} else {b"\n"}),"{name} must use {ending}");
                        if ending=="LF" { assert!(!bytes.contains(&b'\r')); }
                    }
                }
            }
            if with_assumptions {
                let values = final_state["assumptions"].as_array().unwrap();
                let line_endings = values.iter().filter(|value| value["source"] == "planning" && value["question"] == "Line endings").collect::<Vec<_>>();
                assert_eq!(line_endings.len(), 2);
                for value in line_endings {
                    assert_eq!(value["state"], "open");
                    let detail = journal.clip_detail(value["clipId"].as_str().unwrap().into()).await?;
                    assert!(detail["assumptions"].as_array().unwrap().iter().any(|other| other["id"] == value["id"]));
                }
                let job = journal.integration_command(crate::integration::Command { op_id: Uuid::now_v7(), action: crate::integration::Action::Open { tracks: None }}).await.map_err(|error|format!("Reviewing real planning assumptions at Commit: {error}"))?;
                assert_eq!(job.assumptions.iter().filter(|review| review.assumption.source == "planning" && review.assumption.question == "Line endings" && review.eligible).count(), 2);
                final_state["assumptionsAtCommit"] = serde_json::to_value(&job.assumptions)?;
            }
            for (path,index,head) in &original_folders {
                assert_eq!(std::fs::read(path.join(".git/index"))?,*index);
                assert_eq!(crate::source_snapshot::clean_project(path).await?.head.to_string(),*head);
            }
            if inherit {
                let db = Connection::open(root.path().join("db"))?;
                let current = journal.plans().await?.pop().unwrap();
                let basis = crate::lineage_store::basis(&db, &current.items[1].track_ref)?;
                assert!(basis.inherits.contains_key(&current.items[0].track_ref));
                final_state["lineage"] = serde_json::to_value(crate::lineage_store::freeze(&db)?)?;
            }
            std::fs::write(
                output.join("result.json"),
                serde_json::to_vec_pretty(&final_state)?,
            )?;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        }
        .await;
        planning.shutdown().await;
        runtime.shutdown().await;
        if let Some(preparation) = preparation {
            preparation.shutdown().await;
        }
        editor.stop().await;
        result?;
        if cross_folder {
            let mounted = reference_root.ok_or("No native reference mount was recorded")?;
            assert!(
                std::fs::read_dir(mounted)?.next().is_none(),
                "The reference is still mounted after worker shutdown."
            );
            std::fs::write(
                output.join("verification.json"),
                serde_json::to_vec_pretty(
                    &json!({"paidTurns":2,"crossFolder":true,"sourceIndexAndHeadUnchanged":true,"savedParentOnly":true,"primaryHasNoParentFile":true,"unmountedAfterShutdown":true}),
                )?,
            )?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn planner_assumptions_follow_actual_turns_and_commit_review() -> Result<()> {
        let scene = started_parent_assumptions(
            false,
            None,
            vec![crate::plans::DraftAssumption {
                what: "Dummy output format".into(),
                default: "JSON".into(),
            }],
            false,
        )
        .await?;
        let state = scene.journal.snapshot().await?;
        assert!(state["planAssumptions"].as_array().unwrap().is_empty());
        assert_eq!(state["assumptions"].as_array().unwrap().len(), 1);
        let value = state["assumptions"][0].clone();
        assert_eq!(value["source"], "planning");
        assert_eq!(value["options"], json!(["JSON"]));
        assert_eq!(value["trackId"], scene.track);
        let input = scene
            .journal
            .read_utterance(scene.plan.items[0].utterance_id.unwrap().to_string())
            .await?;
        assert!(
            input["text"]
                .as_str()
                .unwrap()
                .contains("Dummy output format: JSON")
        );
        let clip = value["clipId"].as_str().unwrap();
        let detail = scene.journal.clip_detail(clip.into()).await?;
        assert_eq!(detail["assumptions"][0]["id"], value["id"]);
        // Later events, further clips, and reconnect snapshots do not duplicate the template.
        finish_parent(&scene, scene.turn, "Result: done").await?;
        assert_eq!(
            scene.journal.snapshot().await?["assumptions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let db = Connection::open(scene.root.path().join("db"))?;
        assert_eq!(
            db.query_row(
                "SELECT state FROM assumption_sources WHERE id=?1",
                [value["sourceId"].as_str().unwrap()],
                |row| row.get::<_, String>(0)
            )?,
            "complete"
        );
        let state = scene.journal.snapshot().await?;
        scene
            .journal
            .bind_session(
                state["session"]["id"].as_str().unwrap().into(),
                scene.plan.items[0].folder_id.clone(),
                "test".into(),
            )
            .await?;
        while let Some(ticket) = scene.journal.claim_assumptions().await? {
            scene
                .journal
                .finish_assumptions(ticket, crate::assumptions::Finish::Complete(vec![]))
                .await?;
        }
        let job = scene
            .journal
            .integration_command(crate::integration::Command {
                op_id: Uuid::now_v7(),
                action: crate::integration::Action::Open {
                    tracks: Some(vec![scene.track.clone()]),
                },
            })
            .await?;
        assert_eq!(job.assumptions.len(), 1);
        assert!(job.assumptions[0].eligible);
        assert_eq!(job.assumptions[0].assumption.id, value["id"]);
        let mut reviewed = job;
        crate::clock::register(&db)?;
        let tx = db.unchecked_transaction()?;
        crate::integration_assumptions::keep(
            &tx,
            &mut reviewed,
            Some(value["id"].as_str().unwrap()),
        )?;
        tx.commit()?;
        assert_eq!(
            scene.journal.clip_detail(clip.into()).await?["assumptions"][0]["state"],
            "kept"
        );
        Ok(())
    }

    #[tokio::test]
    async fn changing_a_planner_assumption_binds_the_correction_to_its_item() -> Result<()> {
        let scene = started_parent_assumptions(
            false,
            None,
            vec![crate::plans::DraftAssumption {
                what: "Dummy output format".into(),
                default: "JSON".into(),
            }],
            false,
        )
        .await?;
        let value = scene.journal.snapshot().await?["assumptions"][0].clone();
        let command = crate::assumptions::Command {
            op_id: Uuid::now_v7(),
            assumption_id: value["id"].as_str().unwrap().parse()?,
            revision: 1,
            answer: crate::assumptions::Answer::Change {
                text: "Use CSV for this dummy output instead.".into(),
            },
        };
        let answer = scene.journal.answer_assumption(command.clone()).await?;
        assert_eq!(scene.journal.answer_assumption(command).await?, answer);
        let input = answer["changeInputId"].as_str().unwrap();
        let db = Connection::open(scene.root.path().join("db"))?;
        assert_eq!(
            super::owner(&db, input)?,
            Some((scene.plan.id, scene.plan.items[0].plan_item_id))
        );
        assert!(crate::plan_continuation::has_pending(
            &db,
            &scene.plan.items[0].plan_item_id.to_string()
        )?);
        let saved = scene.journal.read_utterance(input.into()).await?;
        assert_eq!(
            saved["target"],
            json!({"mode":"lock","trackId":scene.track})
        );
        assert!(saved["text"].as_str().unwrap().contains("Use CSV"));
        finish_parent(&scene, scene.turn, "Result: done").await?;
        let current = scene.journal.plans().await?.pop().unwrap();
        assert!(current.items[1].utterance_id.is_none());
        assert_eq!(
            scene.journal.snapshot().await?["assumptions"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        Ok(())
    }
    #[tokio::test]
    async fn kept_planning_defaults_remain_settled_after_a_real_turn_is_recorded() -> Result<()> {
        let scene = started_parent_assumptions(
            false,
            None,
            vec![crate::plans::DraftAssumption {
                what: "Dummy format".into(),
                default: "JSON".into(),
            }],
            true,
        )
        .await?;
        assert!(
            scene.journal.snapshot().await?["assumptions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let db = Connection::open(scene.root.path().join("db"))?;
        let raw: String = db.query_row("SELECT record FROM assumptions", [], |row| row.get(0))?;
        let value: serde_json::Value = serde_json::from_str(&raw)?;
        let clip = value["clipId"].as_str().unwrap();
        assert_eq!(value["state"], "kept");
        assert!(value["keptAt"].is_i64());
        finish_parent(&scene, scene.turn, "Result: done").await?;
        assert_eq!(
            scene.journal.clip_detail(clip.into()).await?["assumptions"][0]["state"],
            "kept"
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM assumptions", [], |row| row
                .get::<_, i64>(0))?,
            1
        );
        Ok(())
    }
}
