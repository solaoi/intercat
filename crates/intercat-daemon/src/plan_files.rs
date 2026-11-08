//! An unstarted item's prepared contribution can be replaced without losing
//! edits made in its workspace since the contribution was prepared.
use crate::{
    Result,
    inheritance::Source,
    lineage::{Ancestors, Basis, Point},
};
use intercat_git::{Merge, Repository};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Origin {
    pub item: Uuid,
    pub track: String,
    pub head: String,
    pub basis: Basis,
    pub initial: bool,
    pub first_points: Vec<Point>,
    pub points: Vec<Point>,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS plan_file_origins(item TEXT NOT NULL,track TEXT NOT NULL,record TEXT NOT NULL,PRIMARY KEY(item,track));")?;
    Ok(())
}
fn get(db: &Connection, item: Uuid, track: &str) -> Result<Option<Origin>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM plan_file_origins WHERE item=?1 AND track=?2",
            params![item.to_string(), track],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}
fn initial_item(db: &Connection, input: &str) -> Result<Option<Uuid>> {
    let item: Option<String> = db
        .query_row(
            "SELECT item FROM plan_inputs WHERE input=?1 AND role='initial'",
            [input],
            |row| row.get(0),
        )
        .optional()?;
    item.map(|item| item.parse().map_err(Into::into))
        .transpose()
}
pub(crate) fn for_input(db: &Connection, input: &str) -> Result<Option<Origin>> {
    let Some(item) = initial_item(db, input)? else {
        return Ok(None);
    };
    let track: Option<String> = db
        .query_row(
            "SELECT json_extract(decision,'$.trackId') FROM utterances WHERE id=?1",
            [input],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    match track {
        Some(track) => get(db, item, &track),
        None => Ok(None),
    }
}
pub(crate) fn has_origin(db: &Connection, item: Uuid, track: &str) -> Result<bool> {
    Ok(get(db, item, track)?.is_some())
}
pub(crate) fn has_files(db: &Connection, item: Uuid, track: &str) -> Result<bool> {
    Ok(get(db, item, track)?.is_some_and(|origin| !origin.points.is_empty()))
}
pub(crate) fn pending(db: &Connection, input: &str) -> Result<Option<Origin>> {
    let Some(origin) = for_input(db, input)? else {
        return Ok(None);
    };
    let desired = crate::inheritance::dependencies(db, input)?
        .map(|source| source.points)
        .unwrap_or_default();
    Ok((origin.points != desired).then_some(origin))
}
fn record(
    db: &Connection,
    input: &str,
    head: &str,
    basis: &Basis,
    initial: bool,
    source: &crate::inheritance::Dependencies,
) -> Result<()> {
    let Some(item) = initial_item(db, input)? else {
        return Ok(());
    };
    let mut origin = get(db, item, &source.track)?.unwrap_or(Origin {
        item,
        track: source.track.clone(),
        head: head.into(),
        basis: basis.clone(),
        initial,
        first_points: source.points.clone(),
        points: vec![],
    });
    for point in &source.points {
        if !origin
            .first_points
            .iter()
            .any(|first| first.track == point.track)
        {
            origin.first_points.push(point.clone());
        }
    }
    origin.points = source.points.clone();
    db.execute("INSERT INTO plan_file_origins VALUES(?1,?2,?3) ON CONFLICT(item,track) DO UPDATE SET record=excluded.record",params![item.to_string(),source.track,serde_json::to_string(&origin)?])?;
    Ok(())
}
pub(crate) fn initial(db: &Connection, value: &crate::inheritance::Initial) -> Result<()> {
    record(
        db,
        &value.dependencies.input,
        &value.original.base,
        &value.original,
        true,
        &value.dependencies,
    )
}
pub(crate) fn applied(
    db: &Connection,
    value: &crate::inheritance_apply_store::Application,
) -> Result<()> {
    record(
        db,
        &value.dependencies.input,
        &value.before,
        &value.before_basis,
        false,
        &value.dependencies,
    )
}
fn compose(repo: &Repository, origin: &Origin, source: &Source) -> Result<(String, Basis)> {
    if source.dependencies.points.is_empty() {
        return Ok((origin.head.clone(), origin.basis.clone()));
    }
    let first = Source {
        dependencies: crate::inheritance::Dependencies {
            input: source.dependencies.input.clone(),
            track: source.dependencies.track.clone(),
            points: source
                .dependencies
                .points
                .iter()
                .map(|point| {
                    origin
                        .first_points
                        .iter()
                        .find(|first| first.track == point.track)
                        .unwrap_or(point)
                        .clone()
                })
                .collect(),
        },
        graph: source.graph.clone(),
    };
    let (head, basis) = if origin.initial {
        crate::inheritance::combine(repo, &first)?
    } else {
        crate::inheritance_apply::merge_update(repo, &first, &origin.head, &origin.basis)?
    };
    if first.dependencies.points == source.dependencies.points {
        Ok((head, basis))
    } else {
        crate::inheritance_apply::merge_update(repo, source, &head, &basis)
    }
}
fn transition(before: &Ancestors, current: &Ancestors, after: &Ancestors) -> Result<Ancestors> {
    let mut result = current.clone();
    for key in before.keys().chain(after.keys()) {
        if before.get(key) == after.get(key) {
            continue;
        }
        if current.get(key) != before.get(key) && current.get(key) != after.get(key) {
            return Err("The inherited lineage changed outside this item. Review the workspace before replacing its dependencies.".into());
        }
        if let Some(value) = after.get(key) {
            result.insert(key.clone(), value.clone());
        } else {
            result.remove(key);
        }
    }
    Ok(result)
}
fn merge(
    repo: &Repository,
    before: &str,
    current: &str,
    after: &str,
    base: bool,
) -> Result<intercat_git::ObjectId> {
    match repo.merge_trees(
        repo.tree_id(before.parse()?)?,
        repo.tree_id(current.parse()?)?,
        repo.tree_id(after.parse()?)?,
    )? {
        Merge::Clean(tree) => Ok(tree),
        Merge::Conflicts(paths) => Err(crate::inheritance_issue::Blocker::Conflict {
            paths: paths.iter().map(ToString::to_string).collect(),
            base,
        }
        .into()),
    }
}
pub(crate) fn recompose(
    repo: &Repository,
    origin: &Origin,
    source: &Source,
    current: &str,
    basis: &Basis,
) -> Result<(String, Basis)> {
    let previous = Source {
        dependencies: crate::inheritance::Dependencies {
            input: source.dependencies.input.clone(),
            track: source.dependencies.track.clone(),
            points: origin.points.clone(),
        },
        graph: source.graph.clone(),
    };
    let (before, old_basis) = compose(repo, origin, &previous)?;
    let (desired, new_basis) = compose(repo, origin, source)?;
    let tree = merge(repo, &before, current, &desired, false)?;
    let base = if old_basis.base == new_basis.base {
        basis.base.clone()
    } else if basis.base == old_basis.base {
        new_basis.base.clone()
    } else {
        let tree = merge(repo, &old_basis.base, &basis.base, &new_basis.base, true)?;
        repo.checkpoint_commit(tree, basis.base.parse()?, "Recompose dependency base")?
            .to_string()
    };
    let next = Basis {
        base,
        base_inherits: transition(
            &old_basis.base_inherits,
            &basis.base_inherits,
            &new_basis.base_inherits,
        )?,
        inherits: transition(&old_basis.inherits, &basis.inherits, &new_basis.inherits)?,
    };
    let head = repo.checkpoint_commit(tree, current.parse()?, "Recompose prepared plan item")?;
    repo.sync_objects(&[head, next.base.parse()?])?;
    Ok((head.to_string(), next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lineage::Inherited;

    fn marks(entries: &[(&str, u64)]) -> Ancestors {
        entries
            .iter()
            .map(|(track, number)| {
                (
                    (*track).into(),
                    Inherited {
                        number: *number,
                        sha: format!("dummy-{track}-{number}"),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn continued_turns_do_not_recompose_or_replace_the_initial_file_recipe() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE utterances(id TEXT PRIMARY KEY, decision TEXT NOT NULL); CREATE TABLE plans(id TEXT PRIMARY KEY);")?;
        crate::plan_execution::install(&db)?;
        install(&db)?;
        let item = Uuid::now_v7();
        let plan = Uuid::now_v7();
        db.execute("INSERT INTO plans VALUES(?1)", [plan.to_string()])?;
        let input = Uuid::now_v7().to_string();
        let continuation = Uuid::now_v7().to_string();
        for (id, role) in [(&input, "initial"), (&continuation, "continuation")] {
            db.execute(
                "INSERT INTO utterances VALUES(?1,?2)",
                params![id, r#"{"trackId":"child"}"#],
            )?;
            db.execute(
                "INSERT INTO plan_inputs VALUES(?1,?2,?3,?4)",
                params![id, plan.to_string(), item.to_string(), role],
            )?;
        }
        let source = crate::inheritance::Dependencies {
            input: input.clone(),
            track: "child".into(),
            points: vec![Point {
                track: "parent".into(),
                number: 1,
                sha: "dummy-parent".into(),
                base_at: "dummy-base".into(),
                base_inherits_at: Ancestors::new(),
                inherits: Ancestors::new(),
            }],
        };
        record(
            &db,
            &input,
            "dummy-base",
            &Basis::root("dummy-base".into()),
            true,
            &source,
        )?;
        assert_eq!(for_input(&db, &input)?.unwrap().points, source.points);
        assert!(for_input(&db, &continuation)?.is_none());
        assert!(pending(&db, &continuation)?.is_none());
        let later = crate::inheritance::Dependencies {
            input: continuation.clone(),
            points: vec![],
            ..source.clone()
        };
        record(
            &db,
            &continuation,
            "later-base",
            &Basis::root("later-base".into()),
            false,
            &later,
        )?;
        assert_eq!(for_input(&db, &input)?.unwrap().points, source.points);
        Ok(())
    }

    #[test]
    fn replacing_dependencies_retains_unrelated_lineage_and_rejects_overwrites() -> Result<()> {
        let before = marks(&[("parent", 1)]);
        let current = marks(&[("parent", 1), ("other", 4)]);
        let after = marks(&[("replacement", 2)]);
        assert_eq!(
            transition(&before, &current, &after)?,
            marks(&[("other", 4), ("replacement", 2)])
        );
        assert!(transition(&before, &marks(&[("parent", 2)]), &after).is_err());
        assert_eq!(
            transition(&before, &marks(&[("replacement", 2)]), &after)?,
            after
        );
        Ok(())
    }

    #[test]
    fn recomposition_keeps_parent_base_updates_and_existing_worker_edits() -> Result<()> {
        let root = tempfile::tempdir_in("/private/tmp")?;
        gix::init(root.path())?;
        let repo = Repository::open(root.path())?;
        let empty = repo.empty_starting_point()?;
        let commit = |parent, files: &[(&str, &str)]| -> Result<_> {
            let entries = files
                .iter()
                .map(|(path, text)| Ok((*path, repo.write_blob(text.as_bytes())?)))
                .collect::<Result<Vec<_>>>()?;
            repo.checkpoint_commit(repo.write_tree(&entries)?, parent, "Dummy dependency tree")
        };
        let parent = commit(empty, &[("parent.txt", "first\n")])?;
        let first = Point {
            track: "parent".into(),
            number: 1,
            sha: parent.to_string(),
            base_at: empty.to_string(),
            base_inherits_at: Ancestors::new(),
            inherits: Ancestors::new(),
        };
        let moved_base = commit(empty, &[("base-update.txt", "base\n")])?;
        let updated = commit(
            moved_base,
            &[("base-update.txt", "base\n"), ("parent.txt", "updated\n")],
        )?;
        let second = Point {
            number: 2,
            sha: updated.to_string(),
            base_at: moved_base.to_string(),
            ..first.clone()
        };
        let own = commit(empty, &[("own.txt", "before\n")])?;
        let mut origin = Origin {
            item: Uuid::now_v7(),
            track: "child".into(),
            head: own.to_string(),
            basis: Basis::root(empty.to_string()),
            initial: false,
            first_points: vec![first.clone()],
            points: vec![first.clone()],
        };
        let mut source = Source {
            dependencies: crate::inheritance::Dependencies {
                input: Uuid::now_v7().to_string(),
                track: "child".into(),
                points: vec![second.clone()],
            },
            graph: crate::lineage::Frozen {
                points: vec![first, second.clone()],
                integrated: Ancestors::new(),
            },
        };
        let previous = Source {
            dependencies: crate::inheritance::Dependencies {
                points: origin.points.clone(),
                ..source.dependencies.clone()
            },
            graph: source.graph.clone(),
        };
        let (old_head, old_basis) = compose(&repo, &origin, &previous)?;
        let current = commit(
            old_head.parse()?,
            &[
                ("parent.txt", "first\n"),
                ("own.txt", "changed\n"),
                ("extra.txt", "keep\n"),
            ],
        )?;
        let (head, basis) = recompose(&repo, &origin, &source, &current.to_string(), &old_basis)?;
        assert_eq!(
            repo.tree_id(basis.base.parse()?)?,
            repo.tree_id(moved_base)?
        );
        let files = repo.saved_files(head.parse()?)?;
        for file in ["base-update.txt", "parent.txt", "own.txt", "extra.txt"] {
            assert!(files.contains_key(file));
        }
        assert_eq!(files["parent.txt"].object, repo.write_blob(b"updated\n")?);
        assert_eq!(files["own.txt"].object, repo.write_blob(b"changed\n")?);
        assert_eq!(files["extra.txt"].object, repo.write_blob(b"keep\n")?);
        assert_eq!(basis.inherits["parent"], second.inherited());
        origin.points = vec![second];
        source.dependencies.points.clear();
        let (removed, removed_basis) = recompose(&repo, &origin, &source, &head, &basis)?;
        let files = repo.saved_files(removed.parse()?)?;
        assert!(!files.contains_key("parent.txt") && !files.contains_key("base-update.txt"));
        assert!(files.contains_key("own.txt") && files.contains_key("extra.txt"));
        assert_eq!(files["own.txt"].object, repo.write_blob(b"changed\n")?);
        assert_eq!(removed_basis, origin.basis);
        Ok(())
    }
}
