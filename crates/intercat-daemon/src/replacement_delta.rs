//! Replacement reverses published deltas, never a worker's whole checkpoint.
use crate::{Result, integration_record::Record};
use intercat_git::{Merge, ObjectId};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MixedKind {
    BorrowedRetiredChanges,
    ExtendsCancelledContext,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Mixed {
    pub kind: MixedKind,
    pub source: String,
    pub dependency: String,
    pub record: Uuid,
    pub track: String,
    pub after: u64,
    pub through: u64,
}
#[derive(Clone, Debug)]
pub enum Selection {
    Ready(Vec<Record>),
    Mixed(Vec<Mixed>),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "kind",
    content = "checkpoint",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum Boundary {
    /// The fork checkpoint remains shared context; later borrowed changes do not.
    Fork(u64),
    /// An unchosen alternative has no shared checkpoint of its own.
    EntireTrack,
}
impl Boundary {
    fn cancel_from(self) -> u64 {
        match self {
            Self::Fork(number) => number,
            Self::EntireTrack => 1,
        }
    }
    pub(crate) fn shared_through(self) -> u64 {
        match self {
            Self::Fork(number) => number,
            Self::EntireTrack => 0,
        }
    }
}
/// The caller reads the immutable receipts of one session and folder. The fork
/// boundary is a checkpoint number; publication positions order inverses across tracks.
pub fn select(
    records: &[Record],
    sources: &BTreeMap<String, Boundary>,
    cancelled: &BTreeSet<Uuid>,
) -> Result<Selection> {
    if sources.is_empty()
        || sources
            .iter()
            .any(|(source, boundary)| source.is_empty() || *boundary == Boundary::Fork(0))
    {
        return Err(
            "Replacement needs identified source tracks and valid fork checkpoints.".into(),
        );
    }
    let mut scope = None;
    let mut ids = BTreeSet::new();
    let mut ordinals = BTreeSet::new();
    let mut positions = BTreeSet::new();
    for record in records {
        record.validate()?;
        let next = (&record.session_id, &record.folder_id);
        if scope.is_some_and(|scope| scope != next)
            || !ids.insert(record.id)
            || !ordinals.insert((&record.track_id, record.ordinal))
            || !positions.insert(record.position)
        {
            return Err("Replacement records have different scopes or repeated identities.".into());
        }
        scope = Some(next);
    }
    let selected = records
        .iter()
        .filter(|record| {
            !cancelled.contains(&record.id)
                && sources
                    .get(&record.track_id)
                    .is_some_and(|boundary| record.checkpoint.number >= boundary.cancel_from())
        })
        .cloned()
        .collect::<Vec<_>>();
    let planned = selected
        .iter()
        .map(|record| record.id)
        .collect::<BTreeSet<_>>();
    let mut mixed = Vec::new();
    for record in records
        .iter()
        .filter(|record| !cancelled.contains(&record.id) && !planned.contains(&record.id))
    {
        for (source, boundary) in sources {
            if let Some(mark) = record.coverage_after.get(source) {
                let before = record
                    .coverage_before
                    .get(source)
                    .map(|mark| mark.number)
                    .unwrap_or(0);
                if mark.number > before && mark.number > boundary.shared_through() {
                    mixed.push(Mixed {
                        kind: MixedKind::BorrowedRetiredChanges,
                        source: source.clone(),
                        dependency: source.clone(),
                        record: record.id,
                        track: record.track_id.clone(),
                        after: before,
                        through: mark.number,
                    });
                }
            }
        }
    }
    // A retired commit can also have supplied another source's context. A
    // later kept commit may only contain that source's subsequent delta. Its
    // coverage cannot describe the hole left by reversing the earlier delta.
    // Require an explicit choice instead of preserving a false coverage mark.
    let mut extensions = BTreeSet::new();
    for removed in &selected {
        for (dependency, through) in &removed.coverage_after {
            let before = removed
                .coverage_before
                .get(dependency)
                .map_or(0, |m| m.number);
            if through.number <= before || sources.contains_key(dependency) {
                continue;
            }
            for kept in records.iter().filter(|record| {
                record.position > removed.position
                    && !cancelled.contains(&record.id)
                    && !planned.contains(&record.id)
            }) {
                let Some(base) = kept.coverage_before.get(dependency) else {
                    continue;
                };
                let Some(end) = kept.coverage_after.get(dependency) else {
                    continue;
                };
                if base.number > before
                    && end.number > base.number
                    && extensions.insert((removed.track_id.clone(), dependency.clone(), kept.id))
                {
                    mixed.push(Mixed {
                        kind: MixedKind::ExtendsCancelledContext,
                        source: removed.track_id.clone(),
                        dependency: dependency.clone(),
                        record: kept.id,
                        track: kept.track_id.clone(),
                        after: base.number,
                        through: end.number,
                    });
                }
            }
        }
    }
    if !mixed.is_empty() {
        return Ok(Selection::Mixed(mixed));
    }
    let mut selected = selected;
    selected.sort_by_key(|record| std::cmp::Reverse(record.position));
    Ok(Selection::Ready(selected))
}

/// Reverse every context advance conveyed by newest-first receipts, including
/// borrowed context, to the boundary before the oldest cancelled contribution.
pub fn rewind(records: &[Record], coverage: &mut crate::lineage::Ancestors) {
    for record in records {
        for (track, after) in &record.coverage_after {
            let before = record.coverage_before.get(track);
            if before == Some(after) {
                continue;
            }
            if let Some(before) = before {
                coverage.insert(track.clone(), before.clone());
            } else {
                coverage.remove(track);
            }
        }
    }
}

/// A conflict retains the successfully reversed prefix and the exact next
/// three-way merge. Resolving it must precede applying the remaining records.
pub struct Reversed {
    pub applied: Vec<Uuid>,
    pub result: Merge,
}
/// Share the caller's resolution scope with surrounding baseline/file merges.
/// The caller must finish that scope after computing the entire plan.
pub fn reverse(
    merges: &mut crate::merge_choices::Merges<'_>,
    current_tree: ObjectId,
    records: &[Record],
) -> Result<Reversed> {
    let repository = merges.repository();
    let mut tree = current_tree;
    let mut applied = Vec::new();
    let mut previous = None;
    let mut scope = None;
    for record in records {
        record.validate()?;
        let next = (record.session_id.as_str(), record.folder_id.as_str());
        if scope.is_some_and(|scope| scope != next) {
            return Err("Undo records must belong to one session and folder.".into());
        }
        scope = Some(next);
        if previous.is_some_and(|position| position <= record.position) {
            return Err("Undo records must be ordered from newest to oldest.".into());
        }
        previous = Some(record.position);
        let sha = record.sha.parse()?;
        let actual = repository.commit_details(sha)?;
        if repository.reference_target(&record.reference)? != Some(sha)
            || actual.parents != vec![record.parent.parse::<ObjectId>()?]
            || actual.tree != record.tree.parse::<ObjectId>()?
        {
            return Err(
                "The published delta no longer matches its protected integration record.".into(),
            );
        }
        match merges.merge_trees(
            actual.tree,
            tree,
            repository.tree_id(record.parent.parse()?)?,
        )? {
            Merge::Clean(next) => {
                tree = next;
                applied.push(record.id);
            }
            conflict @ Merge::Conflicts { .. } => {
                return Ok(Reversed {
                    applied,
                    result: conflict,
                });
            }
        }
    }
    Ok(Reversed {
        applied,
        result: Merge::Clean(tree),
    })
}
