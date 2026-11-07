//! Page missing view events before releasing live notifications to a worker.
use crate::{
    Result,
    clock::Stamp,
    journal::Journal,
    msp::{Connection, Event},
};
use muse_code_sdk::msp::{ViewPageAnchor, ViewPageDirection, ViewPageParams};
use serde_json::{Value, json};
use std::{collections::HashSet, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Gap {
    pub after: String,
    pub next: String,
    pub through: i64,
    pub targets: Vec<String>,
}

#[derive(Clone, Copy)]
pub struct Context<'a> {
    pub journal: &'a Journal,
    pub track: &'a str,
    pub session: &'a str,
    pub scanner: Option<&'a crate::secret_scan::Scanner>,
}

impl Context<'_> {
    pub(crate) async fn display(&self, frame: Value) -> Result<Value> {
        match self.scanner {
            Some(scanner) => crate::recording::redact(scanner, frame).await,
            None => Ok(crate::recording::sanitize(frame, &[])),
        }
    }
    async fn record_received(&self, event: Option<Event>) -> Result<crate::msp::Received> {
        let mut received = match event {
            Some(Event::Frame(received)) => received,
            Some(Event::Closed(reason)) => return Err(reason.into()),
            None => return Err("The host closed during state recovery.".into()),
        };
        if received.source.is_none() {
            received.source = Some(
                self.journal
                    .msp_raw(
                        self.track.into(),
                        self.session.into(),
                        received.stamp.clone(),
                        self.display(received.frame.clone()).await?.into(),
                        Some(received.lease.clone()),
                    )
                    .await?,
            );
        }
        Ok(received)
    }
}

pub(crate) struct Reply {
    pub value: Value,
    pub source: i64,
}
#[derive(Debug)]
pub(crate) struct Rejected(pub Value);
impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "State recovery request failed (code {}).",
            self.0["code"]
        )
    }
}
impl std::error::Error for Rejected {}

/// Preserve physical receive order while a recovery read or exact-target
/// reclaim is in flight. The execution loop sees held notifications later.
pub(crate) async fn exchange(
    connection: &mut Connection,
    context: Context<'_>,
    method: &str,
    params: Value,
) -> Result<Reply> {
    let request = connection.client.query(method, params).await?;
    exchange_request(connection, context, request).await
}

pub(crate) async fn replay_start(
    connection: &mut Connection,
    context: Context<'_>,
    params: Value,
    guard: crate::msp::StartGuard,
) -> Result<Reply> {
    let prepared = context
        .journal
        .screen_replay(params, context.track.into(), context.session.into())
        .await?;
    let request = connection.client.query_start(prepared, guard).await?;
    exchange_request(connection, context, request).await
}

async fn exchange_request(
    connection: &mut Connection,
    context: Context<'_>,
    mut request: crate::msp::PendingRequest,
) -> Result<Reply> {
    let id = request.id;
    let mut held = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let received = tokio::select! {
                _ = request.cancelled() => return Err(crate::msp::StartCancelled.into()),
                received = connection.next_event() => received,
            };
            // Once a frame was consumed, persist it without racing the local
            // cancellation signal. Cancellation may end the next receive wait.
            let received = context.record_received(received).await?;
            if received.frame["id"].as_u64() == Some(id) && received.frame.get("method").is_none() {
                if let Some(error) = received.frame.get("error") {
                    return Err(Rejected(error.clone()).into());
                }
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(Reply {
                    value: received.frame["result"].clone(),
                    source: received.source.unwrap(),
                });
            }
            if received.frame.get("method").is_some() {
                held.push(Event::Frame(received));
            }
        }
    })
    .await
    .map_err(|_| -> Box<dyn std::error::Error + Send + Sync> {
        "Timed out while reconciling the session.".into()
    })
    .and_then(|result| result);
    connection.restore_events(held);
    result
}

/// A full snapshot from resume establishes a cut before its streamed suffix.
pub async fn resume(
    connection: &mut Connection,
    context: Context<'_>,
    command: uuid::Uuid,
) -> Result<Value> {
    let covered_gap = context
        .journal
        .view_gap(context.track.into(), context.session.into())
        .await?
        .map(|gap| gap.through);
    let reply=exchange(connection,context,"session/resume",json!({"commandId":command,"sessionId":context.session,"excludeItems":false,"history":"snapshot"})).await?;
    if reply.value["session"]["sessionId"] != context.session
        || reply.value["session"]["permissionState"]["profileId"] != "intercat-worker"
    {
        return Err("The resumed snapshot does not match the worker session and profile.".into());
    }
    context
        .journal
        .opening_snapshot(
            context.track.into(),
            context.session.into(),
            reply.source,
            covered_gap,
        )
        .await?;
    Ok(reply.value)
}

/// No input is dispatched by this routine. Pages are applied transactionally,
/// while live frames retain their receive-budget leases until delivered later.
pub async fn recover(
    connection: &mut Connection,
    context: Context<'_>,
    head: &str,
    focus: Option<uuid::Uuid>,
) -> Result<()> {
    let mut held = Vec::new();
    let mut focused = Vec::new();
    let result = walk(connection, &context, head, &mut held, focus, &mut focused).await;
    if result.is_ok() {
        let mut events: Vec<_> = focused.into_iter().map(Event::Frame).collect();
        events.extend(held);
        held = events;
    }
    connection.restore_events(held);
    result
}

async fn walk(
    connection: &mut Connection,
    context: &Context<'_>,
    head: &str,
    held: &mut Vec<Event>,
    focus: Option<uuid::Uuid>,
    focused: &mut Vec<crate::msp::Received>,
) -> Result<()> {
    if head.is_empty() {
        return Err("The recovered view has no head cursor.".into());
    }
    let mut cursor = context
        .journal
        .view_cursor(context.track.into(), context.session.into())
        .await?;
    let pending = context
        .journal
        .view_gap(context.track.into(), context.session.into())
        .await?;
    // A persisted hole is the authoritative lower bound, even if a later
    // page or live delivery advanced the ordinary saved position before death.
    if let Some(gap) = &pending {
        cursor = if context
            .journal
            .view_seen(context.session.into(), gap.after.clone())
            .await?
        {
            Some(gap.after.clone())
        } else {
            context.journal.view_floor(context.session.into()).await?
        };
    }
    if cursor.as_deref() == Some(head) && pending.is_none() {
        return Ok(());
    }
    let mut target = pending
        .as_ref()
        .map_or_else(|| head.to_owned(), |gap| gap.next.clone());
    let mut through = pending.as_ref().map(|gap| gap.through);
    let mut head_reached = cursor.as_deref() == Some(head);
    let mut reached = false;
    let mut targets: HashSet<_> = pending.into_iter().flat_map(|gap| gap.targets).collect();
    targets.insert(head.to_owned());
    targets.insert(target.clone());
    let mut requested = HashSet::new();
    let mut anchored = false;
    let mut used_anchor = false;
    loop {
        let mut rebase = None;
        let params = ViewPageParams {
            session_id: context.session.into(),
            cursor: cursor.clone(),
            anchor: anchored.then_some(ViewPageAnchor::LatestCompaction),
            direction: Some(ViewPageDirection::Forward),
            limit: 200,
            max_bytes: Some(512 * 1024),
        };
        let request_target = target.clone();
        let request = connection
            .client
            .query("view/page", serde_json::to_value(params)?)
            .await?;
        let id = request.id;
        let (reply, source) = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let received=context.record_received(connection.next_event().await).await?;
                let source=received.source.unwrap();
                if received.frame["id"].as_u64() == Some(id) && received.frame.get("method").is_none() {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>((received.frame, source));
                }
                if received.frame["params"]["sessionId"] == context.session {
                    if received.frame["method"] == "session/viewHealthChanged" {
                        return Err("The host view changed health during recovery. Reconnect before sending work.".into());
                    }
                    if received.frame["method"] == "view/gap" {
                        if received.frame["params"]["after"].as_str().is_none_or(|s|s.is_empty()) || received.frame["params"]["next"].as_str().is_none_or(|s|s.is_empty()) {
                            return Err("Delivery gap has invalid bounds.".into());
                        }
                        if through.is_none() {
                            // This is the first hole discovered during an
                            // ordinary read. Its lower bound can precede that
                            // read's cursor. Preserve this page, then fill from
                            // the earliest saved bound before acknowledging it.
                            rebase=Some(context.journal.view_gap(context.track.into(),context.session.into()).await?.ok_or("Gap receipt has no saved bounds.")?.after);
                        }
                        through=Some(through.map_or(source,|previous|previous.max(source)));
                        if let Some(next) = received.frame["params"]["next"].as_str()
                            && targets.insert(next.to_owned()) {
                                target = next.to_owned();
                                reached = false;
                                context.journal.try_msp(context.track.into(),context.session.into(),received.stamp,context.display(received.frame).await?,Some(received.lease),Some(source))?.await??;
                            }
                        continue;
                    }
                }
                if received.frame.get("method").is_some() { held.push(Event::Frame(received)); }
            }
        }).await.map_err(|_| "Timed out reading the missing history.")??;
        // A coalesced gap changes the target, not the valid page already in
        // flight. Keep its prefix and continue from its last cursor.
        if let Some(error) = reply.get("error") {
            if matches!(error["code"].as_i64(), Some(-32011 | -32040 | -32042))
                && cursor.is_some()
                && !used_anchor
            {
                anchored = true;
                used_anchor = true;
                cursor = None;
                continue;
            }
            return Err(format!(
                "The host could not recover its history (code {}).",
                error["code"]
            )
            .into());
        }
        let page = &reply["result"];
        let events = page["events"]
            .as_array()
            .ok_or("History page has no events.")?;
        let next = match page.get("nextCursor") {
            Some(Value::Null) => None,
            Some(Value::String(next)) if !next.is_empty() => Some(next.clone()),
            _ => return Err("History page has no valid next cursor.".into()),
        };
        if let Some(next) = &next
            && (Some(next) == cursor.as_ref()
                || events
                    .last()
                    .and_then(|event| event["params"]["viewCursor"].as_str())
                    != Some(next))
        {
            return Err("The history page did not advance.".into());
        }
        if anchored {
            let boundary = page["resolvedAnchor"]["boundaryCursor"]
                .as_str()
                .ok_or("The host did not identify its compaction boundary.")?;
            context
                .journal
                .view_compacted(
                    context.track.into(),
                    context.session.into(),
                    boundary.into(),
                    source,
                )
                .await?;
            anchored = false;
        }
        let mut last = None;
        for event in events {
            let method = event["method"]
                .as_str()
                .ok_or("History event has no method.")?;
            let params = event["params"]
                .as_object()
                .ok_or("History event has no parameters.")?;
            if params.get("sessionId").and_then(Value::as_str) != Some(context.session) {
                return Err("History page belongs to another session.".into());
            }
            let position = crate::recording::view_cursor(event)
                .filter(|s| !s.is_empty())
                .ok_or("History event has no durable cursor.")?;
            reached |= position == target;
            head_reached |= position == head;
            last = Some(position.to_owned());
            let mut frame = context
                .display(json!({"jsonrpc":"2.0","method":method,"params":params}))
                .await?;
            if frame["method"] != method
                || frame["params"]["sessionId"] != context.session
                || frame["params"]["viewCursor"] != position
            {
                return Err("History used sensitive data as a technical identifier.".into());
            }
            // Page events omit their original delivery timestamp. Do not
            // present reconstructed intervals as precise measurements.
            frame["_intercatRecovered"] = json!(true);
            let turn = frame["params"]["turnId"]
                .as_str()
                .or_else(|| frame["params"]["item"]["turnId"].as_str())
                .and_then(|id| id.parse::<uuid::Uuid>().ok());
            if focus.is_some()
                && turn == focus
                && (method == "turn/completed"
                    || method == "item/completed"
                        && frame["params"]["item"]["messagePhase"] == "finalAnswer")
            {
                focused.retain(|old| old.frame["method"] != method);
                let size = serde_json::to_vec(&frame)?.len();
                let flow = connection.client.output_budget();
                let Some(lease) = flow.reserve(size) else {
                    flow.interrupt();
                    let _ = connection.client.interrupt_now(context.session, focus);
                    return Err("Recovered output exceeds the receive budget.".into());
                };
                focused.push(crate::msp::Received {
                    frame: frame.clone(),
                    stamp: Stamp::now(),
                    lease,
                    source: Some(source),
                    input_engaged: false,
                    recorded_display: None,
                });
            }
            context
                .journal
                .try_msp(
                    context.track.into(),
                    context.session.into(),
                    Stamp::now(),
                    frame,
                    None,
                    Some(source),
                )?
                .await??;
        }
        if let Some(after) = rebase {
            cursor = if context
                .journal
                .view_seen(context.session.into(), after.clone())
                .await?
            {
                Some(after)
            } else {
                context.journal.view_floor(context.session.into()).await?
            };
            reached = false;
            head_reached = cursor.as_deref() == Some(head);
            continue;
        }
        if reached && head_reached || next.is_none() && request_target == target {
            break;
        }
        let next = next
            .or(last)
            .or(cursor.clone())
            .ok_or("The history page did not advance.")?;
        if events.is_empty() && request_target == target
            || Some(&next) == cursor.as_ref() && request_target == target
            || !requested.insert((next.clone(), target.clone()))
        {
            return Err("The history page did not advance.".into());
        }
        cursor = Some(next);
    }
    // The same cursor can arrive in both a page and the subscription. Its raw
    // receipt is retained, but it must not re-open a completed approval.
    let mut delivered = Vec::with_capacity(held.len());
    for event in held.drain(..) {
        let Event::Frame(received) = &event else {
            delivered.push(event);
            continue;
        };
        if let Some(position) = crate::recording::view_cursor(&received.frame)
            && received.frame["params"]["sessionId"] == context.session
            && !matches!(
                received.frame["method"].as_str(),
                Some("approval/request" | "userInput/request")
            )
            && context
                .journal
                .view_seen(context.session.into(), position.into())
                .await?
        {
            continue;
        }
        delivered.push(event);
    }
    *held = delivered;
    if let Some(through) = through {
        context
            .journal
            .gap_filled(context.track.into(), context.session.into(), through)
            .await?;
    }
    Ok(())
}
