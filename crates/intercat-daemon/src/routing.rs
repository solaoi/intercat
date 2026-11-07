//! Local candidate selection. The model only sees eight numbered tracks.
use crate::{Result, decision::Candidate};
use rusqlite::Connection;
use std::collections::{BTreeSet, HashMap, HashSet};

pub struct Track {
    pub candidate: Candidate,
    pub spoken_at: Option<i64>,
}

pub struct Selection {
    pub first: Vec<Candidate>,
    pub expanded: Vec<Candidate>,
}

fn mentions_label(text: &str, label: &str) -> bool {
    !label.is_empty()
        && text.match_indices(label).any(|(start, _)| {
            let before = text[..start].chars().next_back();
            let after = text[start + label.len()..].chars().next();
            !before.is_some_and(|c| c.is_ascii_alphanumeric())
                && !after.is_some_and(|c| c.is_ascii_alphanumeric())
        })
}

pub fn select(
    connection: &Connection,
    text: &str,
    mut tracks: Vec<Track>,
    selected: Option<&str>,
    locked: Option<&str>,
    needs_you: &[String],
) -> Result<Selection> {
    crate::routing_index::refresh(connection, &tracks)?;
    let mut matches = Vec::new();
    let mut activities = HashMap::new();
    for (id, activity) in crate::routing_index::search(connection, text)? {
        if let Some(index) = tracks.iter().position(|track| track.candidate.id == id) {
            matches.push(index);
            if let Some(activity) = activity {
                activities.insert(index, activity);
            }
        }
    }
    let mut indices = Vec::new();
    let mut seen = HashSet::new();
    let mut push = |index| {
        if indices.len() < 8 && seen.insert(index) {
            indices.push(index);
        }
    };
    let find = |id: &str| tracks.iter().position(|track| track.candidate.id == id);
    if let Some(index) = locked.and_then(find) {
        push(index);
    }
    let lowercase = text.to_lowercase();
    for (index, track) in tracks.iter().enumerate() {
        if mentions_label(text, &track.candidate.label)
            || (!track.candidate.title.is_empty()
                && lowercase.contains(&track.candidate.title.to_lowercase()))
        {
            push(index);
        }
    }
    if let Some(index) = selected.and_then(find) {
        push(index);
    }
    for index in needs_you.iter().filter_map(|id| find(id)).take(2) {
        push(index);
    }
    let mut recent: Vec<_> = tracks
        .iter()
        .enumerate()
        .filter_map(|(index, track)| track.spoken_at.map(|at| (index, at)))
        .collect();
    recent.sort_by_key(|&(index, at)| (std::cmp::Reverse(at), index));
    for &(index, _) in recent.iter().take(5) {
        push(index);
    }
    for &index in &matches {
        push(index);
    }
    // A second pass prioritizes candidates outside the rejected first set.
    // When search has no match, all remaining tracks are still eligible.
    let rejected: HashSet<_> = indices.iter().copied().collect();
    let mut expanded = Vec::new();
    let mut included = HashSet::new();
    for index in matches
        .iter()
        .copied()
        .chain(0..tracks.len())
        .filter(|index| !rejected.contains(index))
        .chain(indices.iter().copied())
    {
        if expanded.len() < 8 && included.insert(index) {
            expanded.push(index);
        }
    }
    let selected_indices: BTreeSet<_> = indices.iter().chain(&expanded).copied().collect();
    for index in selected_indices {
        tracks[index].candidate.activity = match activities.remove(&index) {
            Some(activity) => Some(activity),
            None => crate::routing_index::latest(connection, &tracks[index].candidate.id)?,
        };
    }
    Ok(Selection {
        first: indices
            .into_iter()
            .map(|index| tracks[index].candidate.clone())
            .collect(),
        expanded: expanded
            .into_iter()
            .map(|index| tracks[index].candidate.clone())
            .collect(),
    })
}
