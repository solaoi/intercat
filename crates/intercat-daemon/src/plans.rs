//! Plan identities, revisions and dependency validation. No model or I/O here.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Draft,
    Held,
    Running,
    Done,
    SingleJob,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ItemState {
    Planned,
    Editing,
    Running,
    Succeeded,
    Failed,
    Cut,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum GenerationState {
    Queued,
    Running,
    Ready,
    TimedOut,
    Invalid,
    UnlockRequired,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Generation {
    pub id: Uuid,
    pub state: GenerationState,
    pub attempt: u8,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Dependency {
    pub id: String,
    pub inherit: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Item {
    pub plan_item_id: Uuid,
    pub track_ref: String,
    pub folder_id: String,
    pub order: usize,
    pub title: String,
    pub instruction: String,
    pub constraints: Vec<String>,
    pub done_when: String,
    pub after: Vec<Dependency>,
    pub skip_order: Vec<Uuid>,
    pub start_requested: bool,
    pub state: ItemState,
    pub utterance_id: Option<Uuid>,
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BasedOn {
    pub folder_id: String,
    pub track_id: Option<String>,
    pub sha: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Plan {
    pub id: Uuid,
    pub rev: u64,
    pub state: State,
    pub source_id: String,
    pub source_revision: i64,
    pub execute_allowed: bool,
    pub start_mode: crate::plan_start::Mode,
    pub locked_track: Option<String>,
    pub(crate) unlock_proposal: Option<Draft>,
    pub generation: Generation,
    pub based_on: Vec<BasedOn>,
    pub attachments: Vec<crate::attachment_store::Attachment>,
    pub summary: String,
    pub notices: Vec<String>,
    pub assumptions: Vec<PlanAssumption>,
    pub items: Vec<Item>,
    pub seen_revision: Option<u64>,
    pub rewrite: Option<crate::plan_rewrite::Edit>,
    pub(crate) feedback: Option<Feedback>,
}
impl Plan {
    pub fn new(
        source: Uuid,
        revision: i64,
        execute_allowed: bool,
        locked_track: Option<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v5(&source, b"plan"),
            rev: 1,
            state: State::Draft,
            source_id: source.to_string(),
            source_revision: revision,
            execute_allowed,
            start_mode: crate::plan_start::Mode::Manual,
            locked_track,
            unlock_proposal: None,
            generation: Generation {
                id: Uuid::now_v7(),
                state: GenerationState::Queued,
                attempt: 0,
                detail: None,
            },
            based_on: vec![],
            attachments: vec![],
            summary: String::new(),
            notices: vec![],
            assumptions: vec![],
            items: vec![],
            seen_revision: None,
            rewrite: None,
            feedback: None,
        }
    }
    pub(crate) fn can_plan(&self) -> bool {
        self.state == State::Draft
            || self
                .rewrite
                .as_ref()
                .is_some_and(|edit| edit.words.is_some())
    }
    pub fn next_revision(&mut self) -> Result<(), String> {
        self.rev = self.rev.checked_add(1).ok_or("Plan revision overflow.")?;
        self.seen_revision = None;
        Ok(())
    }
    pub fn require_revision(&self, expected: u64) -> Result<(), String> {
        if self.rev != expected {
            Err("The plan changed · review again".into())
        } else {
            Ok(())
        }
    }
    pub fn apply_draft(&mut self, draft: Draft, context: &Context) -> Result<(), Vec<String>> {
        if self
            .items
            .iter()
            .any(|item| matches!(item.state, ItemState::Running | ItemState::Succeeded))
        {
            return Err(vec!["The plan has already started.".into()]);
        }
        let (items, notices) = if self.feedback.is_some() {
            materialize_revision(self, &draft, context)?
        } else {
            materialize(self.id, self.locked_track.as_deref(), &draft, context)?
        };
        self.next_revision().map_err(|error| vec![error])?;
        self.assumptions = draft
            .assumptions
            .iter()
            .map(|value| {
                self.assumptions
                    .iter()
                    .find(|old| old.what == value.what && old.default == value.default)
                    .cloned()
                    .unwrap_or_else(|| PlanAssumption {
                        id: Uuid::new_v5(&self.generation.id, &serde_json::to_vec(value).unwrap()),
                        what: value.what.clone(),
                        default: value.default.clone(),
                        kept_at: None,
                    })
            })
            .collect();
        self.summary = draft.summary;
        self.items = items;
        self.notices = notices;
        self.generation.state = GenerationState::Ready;
        self.generation.detail = None;
        self.unlock_proposal = None;
        if let Some(feedback) = &mut self.feedback {
            feedback.pending = false;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Feedback {
    pub instruction: String,
    pub pending: bool,
    pub fallback: Option<Fallback>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Fallback {
    pub summary: String,
    pub items: Vec<Item>,
    pub removed_titles: Vec<String>,
    pub pending_correction: Option<String>,
}

/// Local IDs in a model reply are scoped to this plan. Persisted IDs are UUIDs
/// derived from the plan and local ID, including when a reply is retried.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DraftAssumption {
    pub what: String,
    pub default: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanAssumption {
    pub id: Uuid,
    pub what: String,
    pub default: String,
    pub kept_at: Option<i64>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Draft {
    pub assumptions: Vec<DraftAssumption>,
    pub conflicts: Vec<DraftConflict>,
    pub summary: String,
    pub items: Vec<DraftItem>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DraftConflict {
    pub track_a: String,
    pub track_b: String,
    pub what: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DraftItem {
    pub id: String,
    pub track_ref: String,
    pub folder_id: String,
    pub title: String,
    pub instruction: String,
    pub constraints: Vec<String>,
    pub done_when: String,
    pub after: Vec<Dependency>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct External {
    pub track_id: String,
    pub folder_id: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Context {
    pub folders: BTreeSet<String>,
    pub tracks: BTreeMap<String, String>,
    pub external: BTreeMap<String, External>,
    pub plan_items: BTreeSet<String>,
    /// Existing explicit dependencies and within-track order, child -> parent.
    pub edges: Vec<(String, String)>,
    pub order_before: Vec<OrderedItem>,
    pub order_after: Vec<OrderedItem>,
    pub inherited_edges: Vec<(String, String)>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct OrderedItem {
    pub id: String,
    pub track: String,
    pub skipped: BTreeSet<String>,
}

pub(crate) fn track_key(plan: Uuid, track: &str) -> String {
    if track.starts_with("new:") {
        format!("{plan}:{track}")
    } else {
        track.into()
    }
}

pub(crate) fn order_edges(items: &[OrderedItem]) -> Vec<(String, String)> {
    let mut prior: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut edges = Vec::new();
    for item in items {
        for previous in prior.entry(&item.track).or_default().iter() {
            if !item.skipped.contains(*previous) {
                edges.push((item.id.clone(), (*previous).into()));
            }
        }
        prior.entry(&item.track).or_default().push(&item.id);
    }
    edges
}

fn new_track(value: &str) -> bool {
    value
        .strip_prefix("new:")
        .and_then(|number| number.parse::<u32>().ok())
        .is_some_and(|number| number > 0)
}

fn valid_track(track: &str, folder: &str, context: &Context) -> bool {
    new_track(track)
        || context
            .tracks
            .get(track)
            .is_some_and(|value| value == folder)
}

pub(crate) fn cycle(edges: &[(String, String)]) -> bool {
    let mut adjacency: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut incoming: BTreeMap<&str, usize> = BTreeMap::new();
    for (child, parent) in edges {
        incoming.entry(parent).or_default();
        if adjacency.entry(parent).or_default().insert(child) {
            *incoming.entry(child).or_default() += 1;
        }
    }
    let mut ready: Vec<_> = incoming
        .iter()
        .filter_map(|(id, count)| (*count == 0).then_some(*id))
        .collect();
    let mut visited = 0;
    while let Some(id) = ready.pop() {
        visited += 1;
        for child in adjacency.get(id).into_iter().flatten() {
            let count = incoming.get_mut(child).unwrap();
            *count -= 1;
            if *count == 0 {
                ready.push(*child);
            }
        }
    }
    visited != incoming.len()
}

pub fn materialize(
    plan: Uuid,
    locked: Option<&str>,
    draft: &Draft,
    context: &Context,
) -> Result<(Vec<Item>, Vec<String>), Vec<String>> {
    let ids = draft
        .items
        .iter()
        .map(|item| (item.id.clone(), Uuid::new_v5(&plan, item.id.as_bytes())))
        .collect();
    assemble(plan, locked, draft, context, ids, &BTreeMap::new())
}

fn materialize_revision(
    plan: &Plan,
    draft: &Draft,
    context: &Context,
) -> Result<(Vec<Item>, Vec<String>), Vec<String>> {
    let known: BTreeMap<_, _> = plan
        .items
        .iter()
        .map(|item| (item.plan_item_id.to_string(), item))
        .collect();
    let ids: BTreeMap<_, _> = draft
        .items
        .iter()
        .map(|item| {
            (
                item.id.clone(),
                known.get(&item.id).map_or_else(
                    || Uuid::new_v5(&plan.id, item.id.as_bytes()),
                    |old| old.plan_item_id,
                ),
            )
        })
        .collect();
    if ids.values().collect::<BTreeSet<_>>().len() != ids.len() {
        return Err(vec![
            "A correction returned duplicate item identities.".into(),
        ]);
    }
    if plan
        .items
        .iter()
        .any(|item| item.state == ItemState::Cut && ids.values().any(|id| *id == item.plan_item_id))
    {
        return Err(vec![
            "A plan correction cannot restore a removed item. Use Restore first.".into(),
        ]);
    }
    let skipped = draft
        .items
        .iter()
        .filter_map(|item| {
            known
                .get(&item.id)
                .filter(|old| old.track_ref == item.track_ref)
                .map(|old| (old.plan_item_id, old.skip_order.clone()))
        })
        .collect();
    let (mut items, notices) = assemble(
        plan.id,
        plan.locked_track.as_deref(),
        draft,
        context,
        ids,
        &skipped,
    )?;
    for old in &plan.items {
        if items
            .iter()
            .all(|item| item.plan_item_id != old.plan_item_id)
        {
            let mut removed = old.clone();
            removed.order = items.len();
            removed.state = ItemState::Cut;
            removed.start_requested = false;
            removed.utterance_id = None;
            removed.detail = None;
            items.push(removed);
        }
    }
    Ok((items, notices))
}

fn validate_conflicts(draft: &Draft, context: &Context) -> Vec<String> {
    let mut errors = Vec::new();
    if draft.conflicts.len() > 32 {
        errors.push("Return at most 32 plan conflicts.".into());
    }
    let tracks: BTreeMap<_, _> = draft
        .items
        .iter()
        .map(|item| (item.track_ref.as_str(), item.folder_id.as_str()))
        .collect();
    let folder = |track: &str| {
        tracks
            .get(track)
            .copied()
            .or_else(|| context.tracks.get(track).map(String::as_str))
    };
    let mut pairs = BTreeSet::new();
    for conflict in &draft.conflicts {
        let mut pair = [&conflict.track_a, &conflict.track_b];
        pair.sort();
        if conflict.track_a == conflict.track_b || !pairs.insert(pair) {
            errors.push("Plan conflicts must name distinct, unrepeated track pairs.".into());
        }
        if !tracks.contains_key(conflict.track_a.as_str())
            && !tracks.contains_key(conflict.track_b.as_str())
            || folder(&conflict.track_a).is_none()
            || folder(&conflict.track_a) != folder(&conflict.track_b)
        {
            errors.push(
                "A plan conflict must involve this plan and two known tracks in one folder.".into(),
            );
        }
        if conflict.what.trim().is_empty() || conflict.what.chars().count() > 1024 {
            errors.push("Each plan conflict needs a description of 1–1024 characters.".into());
        }
    }
    errors
}

fn assemble(
    plan: Uuid,
    locked: Option<&str>,
    draft: &Draft,
    context: &Context,
    ids: BTreeMap<String, Uuid>,
    skipped: &BTreeMap<Uuid, Vec<Uuid>>,
) -> Result<(Vec<Item>, Vec<String>), Vec<String>> {
    let mut errors = Vec::new();
    if draft.summary.trim().is_empty() || draft.items.is_empty() {
        errors.push("A plan needs a summary and at least one item.".into());
    }
    let mut seen_ids = BTreeSet::new();
    let mut local = BTreeMap::new();
    let mut folders = BTreeMap::new();
    for item in &draft.items {
        local.insert(&item.id, item);
        if item.id.is_empty() || item.id.len() > 128 || !seen_ids.insert(item.id.clone()) {
            errors.push("Plan item IDs must be nonempty and unique.".into());
        }
        if !context.folders.contains(&item.folder_id) {
            errors.push(format!("Unknown folder for {}.", item.id));
        }
        if !valid_track(&item.track_ref, &item.folder_id, context) {
            errors.push(format!("Unknown track or wrong folder for {}.", item.id));
        }
        if let Some(previous) = folders.insert(item.track_ref.clone(), item.folder_id.clone())
            && previous != item.folder_id
        {
            errors.push("Items on one track must use the same folder.".into());
        }
        if locked.is_some_and(|track| item.track_ref != track) {
            errors.push("Keep every item on the locked track.".into());
        }
        if item.title.len() > 4096
            || item.title.chars().any(char::is_control)
            || crate::privacy::display_length(&item.title) > 30
        {
            errors.push(format!(
                "Item {} needs a title of at most 30 displayed characters, on one line.",
                item.id
            ));
        }
        if [&item.title, &item.instruction, &item.done_when]
            .iter()
            .any(|text| text.trim().is_empty())
            || item.constraints.iter().any(|text| text.trim().is_empty())
        {
            errors.push(format!(
                "Item {} needs a title, instruction and completion condition.",
                item.id
            ));
        }
    }
    errors.extend(validate_conflicts(draft, context));
    let mut questions = BTreeSet::new();
    if draft.assumptions.len() > 32 {
        errors.push("Return at most 32 plan assumptions.".into());
    }
    for value in &draft.assumptions {
        if [&value.what, &value.default]
            .iter()
            .any(|text| text.trim().is_empty() || text.chars().count() > 1024)
            || !questions.insert(value.what.trim())
        {
            errors.push(
                "Plan assumptions need distinct questions and values of 1–1024 characters.".into(),
            );
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let mut notices = Vec::new();
    let mut items = Vec::new();
    let mut edges = context.edges.clone();
    let mut tracks_by_item: BTreeMap<String, String> = context
        .external
        .iter()
        .map(|(id, item)| (id.clone(), item.track_id.clone()))
        .collect();
    for (local_id, id) in &ids {
        tracks_by_item.insert(id.to_string(), track_key(plan, &local[local_id].track_ref));
    }
    let mut inherited_tracks = Vec::new();
    for (child, parent) in &context.inherited_edges {
        match (tracks_by_item.get(child), tracks_by_item.get(parent)) {
            (Some(child), Some(parent)) if child != parent => {
                inherited_tracks.push((child.clone(), parent.clone()))
            }
            (Some(_), Some(_)) => {}
            _ => errors.push("An existing plan depends on an unavailable item.".into()),
        }
    }
    for (order, item) in draft.items.iter().enumerate() {
        let id = ids[&item.id];
        let id_text = id.to_string();
        let skip = skipped.get(&id).cloned().unwrap_or_default();
        let mut after = Vec::new();
        let mut parents = BTreeSet::new();
        for dependency in &item.after {
            if dependency.id == item.id {
                errors.push(format!("Item {} depends on itself.", item.id));
                continue;
            }
            if !parents.insert(&dependency.id) {
                errors.push(format!("Repeated dependency for {}.", item.id));
                continue;
            }
            let (parent, parent_track, parent_folder) =
                if let Some(parent) = ids.get(&dependency.id) {
                    let source = local[&dependency.id];
                    (
                        parent.to_string(),
                        track_key(plan, &source.track_ref),
                        source.folder_id.clone(),
                    )
                } else if let Some(source) = context.external.get(&dependency.id) {
                    (
                        dependency.id.clone(),
                        source.track_id.clone(),
                        source.folder_id.clone(),
                    )
                } else {
                    errors.push(format!("Unknown dependency for {}.", item.id));
                    continue;
                };
            let inherit = dependency.inherit && parent_folder == item.folder_id;
            if dependency.inherit && !inherit {
                notices.push(format!(
                    "{} waits for {} without inheriting files from another folder.",
                    item.title, dependency.id
                ));
            }
            if inherit && track_key(plan, &item.track_ref) != parent_track {
                inherited_tracks.push((track_key(plan, &item.track_ref), parent_track));
            }
            edges.push((id_text.clone(), parent.clone()));
            after.push(Dependency {
                id: parent,
                inherit,
            });
        }
        items.push(Item {
            plan_item_id: id,
            track_ref: item.track_ref.clone(),
            folder_id: item.folder_id.clone(),
            order,
            title: item.title.clone(),
            instruction: item.instruction.clone(),
            constraints: item.constraints.clone(),
            done_when: item.done_when.clone(),
            after,
            skip_order: skip,
            start_requested: false,
            state: ItemState::Planned,
            utterance_id: None,
            detail: None,
        });
    }
    let mut ordering = context.order_before.clone();
    ordering.extend(items.iter().map(|item| OrderedItem {
        id: item.plan_item_id.to_string(),
        track: track_key(plan, &item.track_ref),
        skipped: item.skip_order.iter().map(ToString::to_string).collect(),
    }));
    ordering.extend(context.order_after.iter().cloned());
    edges.extend(order_edges(&ordering));
    if cycle(&edges) {
        errors.push("The plan has a dependency or track-order cycle.".into());
    }
    if cycle(&inherited_tracks) {
        errors.push("File inheritance must not form a cycle between tracks.".into());
    }
    if errors.is_empty() {
        Ok((items, notices))
    } else {
        Err(errors)
    }
}

/// Use the same graph and folder rules for edits while preserving item IDs.
pub(crate) fn validate_edit(plan: &mut Plan, context: &Context) -> Result<(), Vec<String>> {
    if plan.items.iter().any(|item| {
        matches!(item.state, ItemState::Planned | ItemState::Editing)
            && !valid_track(&item.track_ref, &item.folder_id, context)
    }) {
        return Err(vec![
            "A planned item has an unavailable track or wrong folder.".into(),
        ]);
    }
    if plan.items.iter().any(|item| {
        item.skip_order.iter().any(|id| {
            *id == item.plan_item_id
                || !(plan.items.iter().any(|parent| parent.plan_item_id == *id)
                    || context.plan_items.contains(&id.to_string())
                        && context
                            .external
                            .get(&id.to_string())
                            .is_some_and(|parent| parent.track_id == item.track_ref))
        })
    }) {
        return Err(vec![
            "A skipped order dependency must name another plan item on its track.".into(),
        ]);
    }
    let mut context = context.clone();
    // Historical items remain graph nodes when their worker has been lost.
    // They cannot make that track available to a future item.
    for item in plan.items.iter().filter(|item| {
        matches!(
            item.state,
            ItemState::Running | ItemState::Succeeded | ItemState::Failed
        )
    }) {
        context
            .tracks
            .entry(item.track_ref.clone())
            .or_insert_with(|| item.folder_id.clone());
    }
    for item in plan
        .items
        .iter()
        .filter(|item| item.state == ItemState::Cut)
    {
        context.external.insert(
            item.plan_item_id.to_string(),
            External {
                track_id: track_key(plan.id, &item.track_ref),
                folder_id: item.folder_id.clone(),
            },
        );
    }
    let mut active: Vec<_> = plan
        .items
        .iter()
        .filter(|item| item.state != ItemState::Cut)
        .collect();
    active.sort_by_key(|item| item.order);
    if active.is_empty() {
        return Ok(());
    }
    let ids = active
        .iter()
        .map(|item| (item.plan_item_id.to_string(), item.plan_item_id))
        .collect();
    let draft = Draft {
        assumptions: vec![],
        conflicts: vec![],
        summary: plan.summary.clone(),
        items: active
            .iter()
            .map(|item| DraftItem {
                id: item.plan_item_id.to_string(),
                track_ref: item.track_ref.clone(),
                folder_id: item.folder_id.clone(),
                title: item.title.clone(),
                instruction: item.instruction.clone(),
                constraints: item.constraints.clone(),
                done_when: item.done_when.clone(),
                after: item.after.clone(),
            })
            .collect(),
    };
    let skipped = plan
        .items
        .iter()
        .map(|item| (item.plan_item_id, item.skip_order.clone()))
        .collect();
    let (validated, notices) = assemble(
        plan.id,
        plan.locked_track.as_deref(),
        &draft,
        &context,
        ids,
        &skipped,
    )?;
    for value in validated {
        plan.items
            .iter_mut()
            .find(|item| item.plan_item_id == value.plan_item_id)
            .unwrap()
            .after = value.after;
    }
    for notice in notices {
        if !plan.notices.contains(&notice) {
            plan.notices.push(notice);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn item(id: &str, track: &str, parent: Option<&str>) -> DraftItem {
        DraftItem {
            id: id.into(),
            track_ref: track.into(),
            folder_id: "folder".into(),
            title: id.into(),
            instruction: format!("Implement {id}."),
            constraints: vec!["Keep the public API.".into()],
            done_when: "Checks pass.".into(),
            after: parent
                .map(|id| {
                    vec![Dependency {
                        id: id.into(),
                        inherit: true,
                    }]
                })
                .unwrap_or_default(),
        }
    }
    fn context() -> Context {
        Context {
            folders: BTreeSet::from(["folder".into(), "other".into()]),
            ..Context::default()
        }
    }
    #[test]
    fn conflicts_require_known_same_folder_pairs_and_explicit_empty_output() {
        let mut draft = Draft {
            assumptions: vec![],
            conflicts: vec![DraftConflict {
                track_a: "new:1".into(),
                track_b: "new:2".into(),
                what: "Both edit index.".into(),
            }],
            summary: "Dummy conflicts".into(),
            items: vec![item("a", "new:1", None), item("b", "new:2", None)],
        };
        let check = |draft: &Draft| materialize(Uuid::now_v7(), None, draft, &context());
        assert!(check(&draft).is_ok());
        draft.items[1].folder_id = "other".into();
        assert!(check(&draft).is_err());
        draft.items[1].folder_id = "folder".into();
        draft.conflicts[0].track_b = "new:3".into();
        assert!(check(&draft).is_err());
        draft.conflicts[0].track_b = "new:1".into();
        assert!(check(&draft).is_err());
        draft.conflicts[0].track_b = "new:2".into();
        draft.conflicts.push(draft.conflicts[0].clone());
        assert!(check(&draft).is_err());
        draft.conflicts.clear();
        let mut value = serde_json::to_value(&draft).unwrap();
        assert!(serde_json::from_value::<Draft>(value.clone()).is_ok());
        value.as_object_mut().unwrap().remove("conflicts");
        assert!(serde_json::from_value::<Draft>(value).is_err());
    }
    #[test]
    fn ids_are_stable_across_retry_and_unique_across_plans() {
        let draft = Draft {
            assumptions: vec![],
            conflicts: vec![],
            summary: "Two jobs".into(),
            items: vec![
                item("first", "new:1", None),
                item("second", "new:2", Some("first")),
            ],
        };
        let id = Uuid::now_v7();
        let one = materialize(id, None, &draft, &context()).unwrap().0;
        let two = materialize(id, None, &draft, &context()).unwrap().0;
        let other = materialize(Uuid::now_v7(), None, &draft, &context())
            .unwrap()
            .0;
        assert_eq!(one[0].plan_item_id, two[0].plan_item_id);
        assert_ne!(one[0].plan_item_id, other[0].plan_item_id);
        assert_eq!(one[1].after[0].id, one[0].plan_item_id.to_string());
    }
    #[test]
    fn skipped_external_order_must_name_a_plan_item_on_the_same_track() {
        let mut context = context();
        context.tracks.insert("A".into(), "folder".into());
        let mut plan = Plan::new(Uuid::now_v7(), 0, true, None);
        plan.apply_draft(
            Draft {
                assumptions: vec![],
                conflicts: vec![],
                summary: "Later plan".into(),
                items: vec![item("later", "A", None)],
            },
            &context,
        )
        .unwrap();
        let parent = Uuid::now_v7();
        plan.items[0].skip_order.push(parent);
        context.external.insert(
            parent.to_string(),
            External {
                track_id: "A".into(),
                folder_id: "folder".into(),
            },
        );
        assert!(validate_edit(&mut plan, &context).is_err());
        context.plan_items.insert(parent.to_string());
        assert!(validate_edit(&mut plan, &context).is_ok());
        context
            .external
            .get_mut(&parent.to_string())
            .unwrap()
            .track_id = "B".into();
        assert!(validate_edit(&mut plan, &context).is_err());
    }

    #[test]
    fn validates_order_cycles_and_inheritance_cycles_separately() {
        let backwards = Draft {
            assumptions: vec![],
            conflicts: vec![],
            summary: "Order".into(),
            items: vec![
                item("first", "new:1", Some("second")),
                item("second", "new:1", None),
            ],
        };
        assert!(
            materialize(Uuid::now_v7(), None, &backwards, &context())
                .unwrap_err()
                .iter()
                .any(|error| error.contains("track-order"))
        );
        let mut chain = Draft {
            assumptions: vec![],
            conflicts: vec![],
            summary: "Chain".into(),
            items: vec![
                item("a1", "new:1", None),
                item("b1", "new:2", Some("a1")),
                item("a2", "new:1", Some("b1")),
            ],
        };
        assert!(
            materialize(Uuid::now_v7(), None, &chain, &context())
                .unwrap_err()
                .iter()
                .any(|error| error.contains("inheritance"))
        );
        chain.items[2].after[0].inherit = false;
        assert!(materialize(Uuid::now_v7(), None, &chain, &context()).is_ok());
    }
    #[test]
    fn cross_folder_inheritance_becomes_an_explicit_order_dependency() {
        let mut draft = Draft {
            assumptions: vec![],
            conflicts: vec![],
            summary: "Folders".into(),
            items: vec![
                item("one", "new:1", None),
                item("two", "new:2", Some("one")),
            ],
        };
        draft.items[1].folder_id = "other".into();
        let (items, notices) = materialize(Uuid::now_v7(), None, &draft, &context()).unwrap();
        assert!(!items[1].after[0].inherit);
        assert_eq!(notices.len(), 1);
    }
    #[test]
    fn failed_validation_never_replaces_the_plan_or_advances_its_revision() {
        let mut plan = Plan::new(Uuid::now_v7(), 1, false, Some("A".into()));
        let mut context = context();
        context.tracks.insert("A".into(), "folder".into());
        let draft = Draft {
            assumptions: vec![],
            conflicts: vec![],
            summary: "Locked".into(),
            items: vec![item("one", "new:1", None)],
        };
        assert!(plan.apply_draft(draft, &context).is_err());
        assert_eq!(plan.rev, 1);
        assert!(plan.items.is_empty());
        plan.apply_draft(
            Draft {
                assumptions: vec![],
                conflicts: vec![],
                summary: "Locked".into(),
                items: vec![item("one", "A", None)],
            },
            &context,
        )
        .unwrap();
        assert_eq!(plan.rev, 2);
        assert!(!plan.execute_allowed);
        assert!(plan.require_revision(1).is_err());
    }
    #[test]
    fn titles_use_the_same_thirty_displayed_character_limit_as_clips() {
        let mut draft = Draft {
            assumptions: vec![],
            conflicts: vec![],
            summary: "Dummy plan".into(),
            items: vec![item("one", "new:1", None)],
        };
        for title in [
            "a".repeat(30),
            "計".repeat(30),
            format!("{}<secret:DEMO_TOKEN>", "計".repeat(29)),
        ] {
            draft.items[0].title = title.clone();
            let (items, _) = materialize(Uuid::now_v7(), None, &draft, &context()).unwrap();
            assert_eq!(items[0].title, title);
            assert_eq!(items[0].instruction, draft.items[0].instruction);
        }
        for title in [
            "a".repeat(31),
            "計".repeat(31),
            format!("{}<secret:DEMO_TOKEN>", "計".repeat(30)),
            "line\nbreak".into(),
            "tab\tname".into(),
            format!("<secret:{}>", "A".repeat(5000)),
        ] {
            draft.items[0].title = title;
            let errors = materialize(Uuid::now_v7(), None, &draft, &context()).unwrap_err();
            assert!(
                errors
                    .iter()
                    .any(|error| error.contains("30 displayed characters"))
            );
        }
    }
    #[test]
    fn editing_cannot_bypass_the_title_bound_or_change_other_configuration() {
        let mut plan = Plan::new(Uuid::now_v7(), 1, false, None);
        plan.apply_draft(
            Draft {
                assumptions: vec![],
                conflicts: vec![],
                summary: "Dummy plan".into(),
                items: vec![item("one", "new:1", None)],
            },
            &context(),
        )
        .unwrap();
        plan.items[0].title = "計".repeat(31);
        let before = serde_json::to_string(&plan).unwrap();
        assert!(validate_edit(&mut plan, &context()).is_err());
        assert_eq!(serde_json::to_string(&plan).unwrap(), before);
    }
}
