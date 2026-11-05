//! Establish session ownership and current pending requests before dispatch.
use crate::{
    Result,
    history::{self, Context},
    msp::{Connection, Event, Received},
    questions::Prompt,
};
use muse_code_sdk::msp::{QueuedInput, TurnRef};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct Operation {
    pub id: Uuid,
    pub kind: String,
    pub state: String,
    pub approval: Option<String>,
    pub requirement: Value,
    pub target_turn: Option<Uuid>,
    pub reclaimed_start: Option<Uuid>,
}
#[derive(Clone, Debug)]
pub struct Local {
    pub view: crate::session_view::SessionView,
    pub operations: BTreeMap<Uuid, Operation>,
    pub compacted: bool,
}
#[derive(Clone, Debug, Default)]
pub struct Expected {
    pub turn: Option<Uuid>,
    pub checkpoint: Option<Uuid>,
    pub checkpoint_pending: bool,
    pub replay: Option<crate::msp::StartGuard>,
}
#[derive(Clone, Debug, Default)]
pub struct State {
    pub active_turn: Option<Uuid>,
    /// Includes a completed turn whose checkpoint still needs saving.
    pub resume_turn: Option<Uuid>,
    pub command: Option<Uuid>,
    pub prompts: Vec<Prompt>,
    pub decided_approvals: BTreeSet<String>,
    pub pending_approvals: usize,
}
pub fn approval_key(id: &str, requirement: &Value) -> String {
    format!("{id}:{}", requirement)
}
impl State {
    pub fn restore_prompts(&self, previous: &[Prompt]) -> Vec<Prompt> {
        self.prompts
            .iter()
            .map(|prompt| {
                let mut current = prompt.clone();
                if let Some(old) = previous.iter().find(|old| {
                    old.request.user_input_id == prompt.request.user_input_id
                        && old.revision == prompt.revision
                }) {
                    current.phase = old.phase;
                    current.operation = old.operation;
                }
                current
            })
            .collect()
    }
}

fn identifier(value: &Value, label: &str) -> Result<Uuid> {
    value
        .as_str()
        .ok_or_else(|| format!("Missing {label}.").into())
        .and_then(|id| id.parse().map_err(Into::into))
}
fn own(local: &Local, command: Uuid) -> Result<()> {
    if !local
        .operations
        .get(&command)
        .is_some_and(|op| op.kind == "turn.start" && op.state != "superseded")
    {
        return Err("The resumed turn has no matching Intercat start. Review its history before continuing.".into());
    }
    Ok(())
}
fn terminal(value: &str) -> bool {
    matches!(value, "completed" | "failed" | "cancelled")
}
fn active(read: &Value, local: &Local) -> Result<Option<(Uuid, Uuid)>> {
    if !local.view.conflicting_starts.is_empty() {
        return Err("The recovered turn has conflicting start identities.".into());
    }
    if local
        .view
        .turns
        .values()
        .any(|t| t.terminal.as_deref().is_some_and(|value| !terminal(value)))
    {
        return Err("The recovered turn has an unknown terminal state.".into());
    }
    let meta = read["session"]
        .get("activeTurnId")
        .ok_or("The host omitted its active turn state.")?;
    let meta = if meta.is_null() {
        None
    } else {
        Some(identifier(meta, "active turn")?)
    };
    let running: Vec<_> = local
        .view
        .turns
        .values()
        .filter(|t| t.state == crate::session_view::TurnState::Running)
        .collect();
    let observed = if let Some(state) = local.view.snapshot_metadata() {
        let current = state
            .get("activeTurn")
            .ok_or("The snapshot omitted its active turn.")?;
        if current.is_null() {
            None
        } else {
            let current: TurnRef = serde_json::from_value(current.clone())?;
            Some((current.turn_id.parse()?, current.command_id.parse()?))
        }
    } else if running.len() == 1 {
        Some((
            running[0].id,
            running[0]
                .command
                .ok_or("The running turn has no start identity.")?,
        ))
    } else if running.is_empty()
        && (read["session"]["turnCount"] == 0
            || !local.compacted && !local.view.turns.is_empty()
            || read["lastTurn"]["terminal"].as_str().is_some_and(terminal))
    {
        None
    } else {
        return Err(
            "Couldn’t rebuild the worker’s active turn. Retry its history or pick up from files."
                .into(),
        );
    };
    if observed.map(|v| v.0) != meta {
        return Err("The host returned conflicting active turn states.".into());
    }
    if running.iter().any(|old| Some(old.id) != meta) {
        return Err(
            "A previous turn still has an unknown outcome. Review its files before continuing."
                .into(),
        );
    }
    if let Some((turn, command)) = observed {
        if let Some(fact) = local.view.turns.get(&turn) {
            if fact.state != crate::session_view::TurnState::Running {
                return Err("The host reports a completed turn as still active.".into());
            }
            if fact.command.is_some_and(|previous| previous != command) {
                return Err("The active turn has conflicting start identities.".into());
            }
        }
        own(local, command)?;
    }
    Ok(observed)
}
fn queued(read: &Value, local: &Local) -> Result<Vec<TurnRef>> {
    if let Some(state) = local.view.snapshot_metadata() {
        return serde_json::from_value(
            state
                .get("queuedTurns")
                .ok_or("The snapshot omitted queued turns.")?
                .clone(),
        )
        .map_err(Into::into);
    }
    let inputs: Vec<QueuedInput> = serde_json::from_value(
        read.get("queuedInputs")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )?;
    if !inputs.is_empty() {
        return Err(
            "Queued input has no exact turn identity. It must be reviewed before dispatch.".into(),
        );
    }
    Ok(vec![])
}
fn pending(value: &Value, session: &str) -> Result<(Vec<Value>, Vec<Prompt>, String)> {
    let approvals = value["approvals"]
        .as_array()
        .ok_or("The host omitted pending approvals.")?
        .clone();
    let inputs = value["userInputs"]
        .as_array()
        .ok_or("The host omitted pending questions.")?;
    if approvals.len() + inputs.len() > 32 {
        return Err("Too many pending requests to recover safely.".into());
    }
    let mut keys = BTreeSet::new();
    let mut normalized = Vec::new();
    for request in &approvals {
        let id = request["approvalId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("Pending approval has no identity.")?;
        if request["sessionId"] != session
            || request.get("currentRequirementId").is_none()
            || !keys.insert(format!("a:{id}"))
        {
            return Err("Pending approvals do not match this session.".into());
        }
        let mut request = request.clone();
        for key in ["viewCursor", "sourceRange"] {
            request
                .as_object_mut()
                .ok_or("Invalid approval.")?
                .remove(key);
        }
        normalized.push(request);
    }
    let mut prompts = Vec::new();
    for request in inputs {
        let prompt = Prompt::parse(request.clone())?;
        if prompt.request.session_id != session
            || !keys.insert(format!("q:{}", prompt.request.user_input_id))
        {
            return Err("Pending questions do not match this session.".into());
        }
        normalized.push(json!({"id":prompt.request.user_input_id,"revision":prompt.revision}));
        prompts.push(prompt);
    }
    normalized.sort_by_key(Value::to_string);
    Ok((approvals, prompts, serde_json::to_string(&normalized)?))
}
fn same_pending(
    read: &Value,
    approvals: &[Value],
    prompts: &[Prompt],
    session: &str,
) -> Result<bool> {
    let requests = read["pendingRequests"]
        .as_array()
        .ok_or("The host omitted pending requests.")?;
    let mut read_approvals = Vec::new();
    let mut read_inputs = Vec::new();
    for request in requests {
        match (request.get("approvalId"), request.get("userInputId")) {
            (Some(_), None) => read_approvals.push(request.clone()),
            (None, Some(_)) => read_inputs.push(request.clone()),
            _ => return Err("The host returned an unknown pending request shape.".into()),
        }
    }
    let (_, _, observed) = pending(
        &json!({"approvals":read_approvals,"userInputs":read_inputs}),
        session,
    )?;
    let (_, _, expected) = pending(
        &json!({"approvals":approvals,"userInputs":prompts.iter().map(|p|&p.request).collect::<Vec<_>>()}),
        session,
    )?;
    Ok(observed == expected)
}

async fn read(connection: &mut Connection, context: Context<'_>) -> Result<history::Reply> {
    let reply = history::exchange(
        connection,
        context,
        "session/read",
        // Resume and the paged view establish history. Repeated reconciliation
        // reads need only live metadata; requesting all items can overflow the
        // host's response limit even after resume reports historyBudget.
        json!({"sessionId":context.session,"excludeItems":true}),
    )
    .await?;
    if reply.value["session"]["sessionId"] != context.session {
        return Err("The host read a different session.".into());
    }
    let cursor = reply.value["viewCursor"]
        .as_str()
        .ok_or("The host read has no view cursor.")?;
    history::recover(connection, context, cursor, None).await?;
    context
        .journal
        .view_snapshot(context.track.into(), context.session.into(), reply.source)
        .await?;
    if let Some(last) = reply.value.get("lastTurn").filter(|value| !value.is_null()) {
        let turn = identifier(&last["turnId"], "last turn")?;
        if !last["terminal"].as_str().is_some_and(terminal) {
            return Err("The host returned an unknown turn outcome.".into());
        }
        let local = context
            .journal
            .reconcile_local(context.session.into())
            .await?;
        match local
            .view
            .turns
            .get(&turn)
            .and_then(|turn| turn.terminal.as_ref())
        {
            // A resumed host may report cancelled after the view already
            // recorded crash_inferred/failed. That turn is already settled.
            Some(_) => {}
            None => {
                context.journal.try_msp(context.track.into(),context.session.into(),crate::clock::Stamp::now(),json!({"jsonrpc":"2.0","method":"turn/completed","params":{"sessionId":context.session,"turnId":turn,"terminal":last["terminal"]},"_intercatRecovered":true}),None,Some(reply.source))?.await??;
            }
        }
    }
    Ok(reply)
}

pub async fn prepare(
    connection: &mut Connection,
    context: Context<'_>,
    expected: Expected,
) -> Result<State> {
    let until = Instant::now() + Duration::from_secs(30);
    let mut reclaims = BTreeMap::<Uuid, Uuid>::new();
    let mut replayed = BTreeSet::new();
    let mut rejected = BTreeSet::new();
    loop {
        if Instant::now() >= until {
            return Err(
                "The host state kept changing during recovery. Retry before sending work.".into(),
            );
        }
        let first = read(connection, context).await?;
        let local = context
            .journal
            .reconcile_local(context.session.into())
            .await?;
        if local
            .operations
            .values()
            .any(|op| op.kind == "turn.unqueue" && op.reclaimed_start.is_none())
        {
            return Err("A saved queued-turn reclaim has no start identity. Review it before sending more work.".into());
        }
        let waiting = queued(&first.value, &local)?;
        for (&turn, &operation) in &reclaims.clone() {
            if first.value["session"]["activeTurnId"] == turn.to_string() {
                return Err("A reclaimed turn unexpectedly started.".into());
            }
            if !waiting
                .iter()
                .any(|queued| queued.turn_id == turn.to_string())
            {
                context
                    .journal
                    .finish(&operation.to_string(), "complete")
                    .await?;
                reclaims.remove(&turn);
            }
        }
        if !waiting.is_empty() {
            for queued in &waiting {
                own(&local, queued.command_id.parse()?)?;
            }
            for queued in waiting {
                let command: Uuid = queued.command_id.parse()?;
                own(&local, command)?;
                let turn: Uuid = queued.turn_id.parse()?;
                let mut existing = local
                    .operations
                    .values()
                    .filter(|op| op.kind == "turn.unqueue" && op.target_turn == Some(turn));
                let op = existing.next().map(|op| op.id).unwrap_or_else(Uuid::now_v7);
                if local
                    .operations
                    .get(&op)
                    .is_some_and(|op| op.reclaimed_start != Some(command))
                {
                    return Err("The saved queued-turn reclaim names a different start.".into());
                }
                if existing.next().is_some() {
                    return Err("The queued turn has conflicting recovery operations.".into());
                }
                if reclaims.contains_key(&turn) {
                    continue;
                }
                if context
                    .journal
                    .operation(op)
                    .await?
                    .is_some_and(|op| op["state"] == "dispatched" || op["state"] == "complete")
                {
                    reclaims.insert(turn, op);
                    continue;
                }
                let params = json!({"commandId":op,"sessionId":context.session,"turnId":turn});
                let mut intent = params.clone();
                intent["reclaimedStart"] = json!(command);
                context
                    .journal
                    .intent(&op.to_string(), "turn.unqueue", intent)
                    .await?;
                let result = history::exchange(connection, context, "turn/unqueue", params)
                    .await?
                    .value;
                if result["commandId"] != op.to_string()
                    || result["turnId"] != turn.to_string()
                    || result["status"] != "accepted"
                {
                    return Err("The host did not confirm the exact queued-turn reclaim.".into());
                }
                context
                    .journal
                    .finish(&op.to_string(), "dispatched")
                    .await?;
                reclaims.insert(turn, op);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            continue;
        }
        let first_pending = history::exchange(
            connection,
            context,
            "approval/listPending",
            json!({"sessionId":context.session}),
        )
        .await?;
        let (_, _, signature) = pending(&first_pending.value, context.session)?;
        let last = read(connection, context).await?;
        let last_pending = history::exchange(
            connection,
            context,
            "approval/listPending",
            json!({"sessionId":context.session}),
        )
        .await?;
        let (approvals, prompts, current) = pending(&last_pending.value, context.session)?;
        let local = context
            .journal
            .reconcile_local(context.session.into())
            .await?;
        if signature != current
            || !same_pending(&last.value, &approvals, &prompts, context.session)?
            || !queued(&last.value, &local)?.is_empty()
        {
            continue;
        }
        let inputs: Vec<QueuedInput> = serde_json::from_value(
            last.value
                .get("queuedInputs")
                .cloned()
                .unwrap_or_else(|| json!([])),
        )?;
        if !inputs.is_empty() {
            return Err(
                "The host still has unclaimed input. Review it before sending more.".into(),
            );
        }
        if approvals.iter().any(|a| {
            a["approvalId"]
                .as_str()
                .is_some_and(|id| local.view.approval_resolved(id))
        }) || prompts
            .iter()
            .any(|p| local.view.user_input_settled(&p.request.user_input_id))
        {
            return Err(
                "A resolved request reappeared in the pending set. Reconnect before answering it."
                    .into(),
            );
        }
        let physical = active(&last.value, &local)?;
        for request in &approvals {
            if physical.map(|current| current.0)
                != Some(identifier(&request["turnId"], "approval turn")?)
            {
                return Err("A pending approval does not belong to the active turn.".into());
            }
        }
        for prompt in &prompts {
            if physical.map(|current| current.0) != Some(prompt.request.turn_id.parse()?) {
                return Err("A pending question does not belong to the active turn.".into());
            }
        }
        let observed: BTreeSet<_> = local
            .view
            .turns
            .values()
            .filter_map(|t| t.command)
            .collect();
        let missing: Vec<_> = local
            .operations
            .values()
            .filter(|op| {
                op.kind == "turn.start"
                    && matches!(op.state.as_str(), "prepared" | "dispatched")
                    && !observed.contains(&op.id)
                    && physical.map(|p| p.1) != Some(op.id)
            })
            .map(|op| op.id)
            .collect();
        let reclaimed = |id| {
            local.operations.values().any(|op| {
                op.kind == "turn.unqueue"
                    && op.state == "complete"
                    && op.reclaimed_start == Some(id)
            })
        };
        let retired: Vec<_> = missing
            .iter()
            .copied()
            .filter(|id| rejected.contains(id) || reclaimed(*id))
            .collect();
        let unresolved: Vec<_> = missing
            .into_iter()
            .filter(|id| !retired.contains(id))
            .collect();
        if !unresolved.is_empty() {
            let guard = expected.replay.as_ref().ok_or("A previous start has no confirmed outcome. Review or retry it before sending more work.")?;
            for command in unresolved {
                if !replayed.insert(command) {
                    return Err("The host acknowledged a previous start but its outcome is still absent from the view. Retry before sending more work.".into());
                }
                let operation = context
                    .journal
                    .operation(command)
                    .await?
                    .ok_or("The previous start is missing.")?;
                let params = operation["payload"].clone();
                if params["commandId"] != command.to_string()
                    || params["sessionId"] != context.session
                {
                    return Err("The previous start parameters do not match the session.".into());
                }
                match history::replay_start(connection, context, params, guard.clone()).await {
                    Ok(reply) => {
                        identifier(&reply.value["turnId"], "replayed turn")?;
                        if !matches!(
                            reply.value["disposition"].as_str(),
                            Some("started" | "queued" | "steered")
                        ) {
                            return Err("The host returned an unknown start disposition.".into());
                        }
                        context
                            .journal
                            .finish(&command.to_string(), "dispatched")
                            .await?;
                    }
                    Err(error)
                        if error
                            .downcast_ref::<history::Rejected>()
                            .is_some_and(|error| {
                                error.0["code"] == -32030
                                    || error.0["data"]["kind"] == "commandRejected"
                            }) =>
                    {
                        rejected.insert(command);
                    }
                    Err(error) => return Err(error),
                }
            }
            continue;
        }
        let mut state = State {
            active_turn: physical.map(|p| p.0),
            resume_turn: physical.map(|p| p.0),
            command: physical.map(|p| p.1),
            prompts,
            pending_approvals: approvals.len(),
            ..State::default()
        };
        if state.resume_turn.is_none() {
            let completed = expected.turn.or_else(|| {
                last.value["lastTurn"]["turnId"]
                    .as_str()
                    .and_then(|id| id.parse().ok())
                    .filter(|id| {
                        expected.checkpoint != Some(*id)
                            && (expected.checkpoint_pending
                                || local
                                    .view
                                    .turns
                                    .get(id)
                                    .and_then(|t| t.command)
                                    .and_then(|c| local.operations.get(&c))
                                    .is_some_and(|op| op.state != "complete"))
                    })
            });
            if let Some(turn) = completed {
                let fact = local
                    .view
                    .turns
                    .get(&turn)
                    .filter(|t| t.terminal.as_deref().is_some_and(terminal))
                    .ok_or("The previous turn outcome is still unknown.")?;
                let command = fact
                    .command
                    .ok_or("The completed turn has no recorded start identity.")?;
                own(&local, command)?;
                state.resume_turn = Some(turn);
                state.command = Some(command);
            }
        }
        if expected.turn.is_some() && expected.turn != state.resume_turn {
            return Err("The recovered turn does not match the previous unfinished turn.".into());
        }
        if expected.checkpoint_pending && state.resume_turn.is_none() {
            return Err("The previous checkpoint is still unresolved. Review its history before continuing.".into());
        }
        for request in &approvals {
            if local.operations.values().any(|op| {
                matches!(op.kind.as_str(), "approval.decide" | "approval/decide")
                    && op.state != "superseded"
                    && op.approval.as_deref() == request["approvalId"].as_str()
                    && op.requirement == request["currentRequirementId"]
            }) {
                state.decided_approvals.insert(approval_key(
                    request["approvalId"].as_str().unwrap(),
                    &request["currentRequirementId"],
                ));
            }
        }
        context.journal.reconcile_record(context.session.into(),json!({"trackId":context.track,"activeTurn":state.active_turn,"resumeTurn":state.resume_turn,"command":state.command,"approvals":approvals.iter().map(|a|json!({"id":a["approvalId"],"requirement":a["currentRequirementId"]})).collect::<Vec<_>>(),"questions":state.prompts.iter().map(|p|json!({"id":p.request.user_input_id,"revision":p.revision})).collect::<Vec<_>>(),"heldStarts":retired}),retired).await?;
        install(connection, context, &state, approvals, last_pending.source).await?;
        return Ok(state);
    }
}

async fn install(
    connection: &mut Connection,
    context: Context<'_>,
    state: &State,
    approvals: Vec<Value>,
    source: i64,
) -> Result<()> {
    let mut events = connection.take_buffered();
    events.retain(|event| match event {
        Event::Frame(received) => {
            received.frame["params"]["sessionId"] != context.session
                || !matches!(
                    received.frame["method"].as_str(),
                    Some(
                        "approval/request"
                            | "approval/requested"
                            | "userInput/request"
                            | "userInput/requested"
                    )
                )
        }
        _ => true,
    });
    let mut frames: Vec<_> = approvals
        .into_iter()
        .map(|params| {
            (
                json!({"jsonrpc":"2.0","method":"approval/request","params":params}),
                source,
            )
        })
        .chain(state.prompts.iter().map(|p| {
            (
                json!({"jsonrpc":"2.0","method":"userInput/request","params":p.request}),
                source,
            )
        }))
        .collect();
    if let Some(turn) = state.resume_turn {
        frames.extend(
            context
                .journal
                .reconcile_replay(context.session.into(), turn)
                .await?,
        );
    }
    for (frame, source) in frames {
        let Some(lease) = connection
            .client
            .output_budget()
            .reserve(serde_json::to_vec(&frame)?.len())
        else {
            connection.restore_events(events);
            return Err("Recovered requests exceed the receive budget.".into());
        };
        events.push(Event::Frame(Received {
            frame,
            stamp: crate::clock::Stamp::now(),
            lease,
            source: Some(source),
            input_engaged: false,
            recorded_display: None,
        }));
    }
    connection.restore_events(events);
    Ok(())
}
