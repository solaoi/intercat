//! Bind read-only references to the checkpoints that released a plan item.
use crate::{
    Result,
    reference_volume::{Recipe, Source},
};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS input_references(input TEXT PRIMARY KEY REFERENCES utterances(id) ON DELETE CASCADE,record TEXT NOT NULL);")?;
    Ok(())
}
pub(crate) fn queue(db: &Connection, input: Uuid, track: &str, sources: Vec<Source>) -> Result<()> {
    if sources.is_empty() {
        return Ok(());
    }
    let recipe = Recipe {
        input,
        track: track.parse()?,
        sources,
    };
    db.execute(
        "INSERT INTO input_references VALUES(?1,?2)",
        params![input.to_string(), serde_json::to_string(&recipe)?],
    )?;
    Ok(())
}
pub(crate) fn read(db: &Connection, input: &str, track: &str) -> Result<Option<Recipe>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM input_references WHERE input=?1",
            [input],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| {
        let recipe: Recipe = serde_json::from_str(&raw)?;
        if recipe.input.to_string() != input || recipe.track.to_string() != track {
            return Err("The dependency references belong to another request.".into());
        }
        Ok(recipe)
    })
    .transpose()
}
pub(crate) fn for_track(db: &Connection, track: &str) -> Result<Vec<Recipe>> {
    let mut query=db.prepare("SELECT record FROM input_references WHERE json_extract(record,'$.track')=?1 ORDER BY rowid")?;
    query
        .query_map([track], |row| row.get::<_, String>(0))?
        .map(|raw| {
            let recipe: Recipe = serde_json::from_str(&raw?)?;
            if recipe.track.to_string() != track {
                return Err("Dependency reference owner changed.".into());
            }
            Ok(recipe)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        decision::{Decision, Kind, Outcome, Target},
        journal::Journal,
        secrets::Key,
    };
    use serde_json::json;
    use std::path::PathBuf;
    #[tokio::test]
    async fn prepared_request_uses_a_saved_readonly_tree_and_remounts_it_after_reopen() -> Result<()>
    {
        // /tmp resolves through /private on macOS; teardown must match the
        // canonical mount path reported by hdiutil after this alias is saved.
        let root = tempfile::tempdir_in("/tmp")?;
        let source = root.path().join("other-project");
        std::fs::create_dir(&source)?;
        gix::init(&source)?;
        let repo = intercat_git::Repository::open(&source)?;
        let empty = repo.empty_starting_point()?;
        let saved = repo.write_blob(b"saved reference\n")?;
        let secret = repo.write_blob(b"dummy private value\n")?;
        let sha = repo.checkpoint_commit(
            repo.write_tree(&[("reference.txt", saved), (".env", secret)])?,
            empty,
            "Dummy reference",
        )?;
        std::fs::write(source.join("reference.txt"), b"new unsaved contents\n")?;
        let work = root.path().join("work");
        std::fs::create_dir(&work)?;
        let database = root.path().join("db");
        let journal = Journal::open(&database, Key::new([7; 32])).await?;
        let input = Uuid::now_v7();
        journal
            .stage_utterance(
                input,
                "Use the saved reference".into(),
                Target::New,
                None,
                vec![],
            )
            .await?;
        let claimed = journal.claim_input().await?.unwrap();
        journal
            .apply_input(
                claimed.id,
                claimed.revision,
                Outcome::Classified(Decision {
                    kind: Kind::Fork,
                    track_id: None,
                    boundary: Some("now".into()),
                    command: None,
                    confidence: 1.,
                    inference_ms: 0.,
                    timed_out: false,
                    scores: json!({}),
                }),
            )
            .await?;
        let word = journal.read_utterance(input.to_string()).await?;
        let track = word["decision"]["trackId"].as_str().unwrap().to_owned();
        let dependency = Source {
            track: Uuid::now_v7().to_string(),
            checkpoint: sha.to_string(),
            number: 1,
            repository: source.clone(),
        };
        let db = Connection::open(&database)?;
        queue(&db, input, &track, vec![dependency.clone()])?;
        assert!(read(&db, &input.to_string(), &Uuid::now_v7().to_string()).is_err());
        let params = json!({"commandId":Uuid::now_v7(),"sessionId":"dummy","input":[{"type":"skill","source":"user","selector":"intercat-report","arguments":"Use the saved reference"}]});
        let result = async {
            let prepared = journal
                .prepare_turn(
                    input.to_string(),
                    track.clone(),
                    "dummy".into(),
                    work.clone(),
                    params.clone(),
                )
                .await?;
            let roots = prepared.params()["workspaceRoots"].as_array().unwrap();
            assert_eq!(roots.len(), 2);
            let mounted = PathBuf::from(roots[1].as_str().unwrap());
            let file = mounted.join(dependency.name()?).join("reference.txt");
            assert_eq!(std::fs::read(&file)?, b"saved reference\n");
            assert!(!mounted.join(dependency.name()?).join(".env").exists());
            assert!(std::fs::write(&file, b"wrong").is_err());
            assert!(
                prepared.params()["input"][0]["arguments"]
                    .as_str()
                    .unwrap()
                    .contains(&sha.to_string())
            );
            assert_eq!(
                std::fs::read(source.join("reference.txt"))?,
                b"new unsaved contents\n"
            );
            drop(prepared);
            journal.unmount_dependency_references(&track).await?;
            std::fs::remove_dir_all(&source)?;
            let reopened = Journal::open(&database, Key::new([7; 32])).await?;
            reopened.mount_dependency_references(&track).await?;
            let prepared = reopened
                .prepare_turn(
                    input.to_string(),
                    track.clone(),
                    "dummy".into(),
                    work,
                    params,
                )
                .await?;
            assert_eq!(
                prepared.params()["workspaceRoots"][1],
                mounted.to_string_lossy().as_ref()
            );
            assert_eq!(std::fs::read(&file)?, b"saved reference\n");
            drop(prepared);
            reopened.unmount_dependency_references(&track).await?;
            db.execute(
                "DELETE FROM input_references WHERE input=?1",
                [input.to_string()],
            )?;
            reopened.mount_dependency_references(&track).await?;
            assert!(
                std::fs::read_dir(&mounted)?.next().is_none(),
                "An obsolete reference was remounted"
            );
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }
        .await;
        journal.unmount_dependency_references(&track).await?;
        if result.is_ok() {
            assert!(
                !root
                    .path()
                    .join("dependency-references")
                    .join(&track)
                    .join(input.to_string())
                    .exists()
            );
        }
        result
    }
}
