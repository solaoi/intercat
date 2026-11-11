//! Immutable shared steps and their object-admission boundary. No Git or user
//! code runs on the writer; a pending pin is replayable after reopening SQLite.
use crate::{
    Result,
    lineage::{Frozen, Views},
    project::Folder,
    shared_context::{Outcome, Projection},
    shared_plan::{self, Rewrite, Saved},
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    pub job: Uuid,
    pub revision: u64,
    pub descendant: String,
    pub parent: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Input {
    pub session: String,
    pub slug: String,
    pub project: Folder,
    pub graph: Frozen,
    pub projection: Projection,
}
impl Input {
    pub(crate) fn id(&self) -> Result<Uuid> {
        let namespace = Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            &serde_json::to_vec(&(&self.session, &self.project.id))?,
        );
        self.projection.identity(namespace)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Phase {
    Pending,
    Pinning,
    NeedsMerge,
    Ready,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Computed {
    pub outcome: Outcome,
    pub anchor: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Record {
    pub id: Uuid,
    pub revision: u64,
    pub input: Input,
    pub phase: Phase,
    pub solutions: Vec<crate::merge_choices::Solved>,
    pub computed: Option<Computed>,
}
pub(crate) enum Preparation {
    Step(Box<Record>),
    Ready(Views),
}
pub(crate) enum Change {
    Begin {
        id: Uuid,
        revision: u64,
        computed: Box<Computed>,
    },
    Admit {
        id: Uuid,
        revision: u64,
    },
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS shared_contexts(id TEXT PRIMARY KEY,session TEXT NOT NULL,folder TEXT NOT NULL,record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS shared_context_scope ON shared_contexts(session,folder);")?;
    Ok(())
}
pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Record> {
    let raw: String = db.query_row(
        "SELECT record FROM shared_contexts WHERE id=?1",
        [id.to_string()],
        |row| row.get(0),
    )?;
    let record: Record = serde_json::from_str(&raw)?;
    if record.id != id || record.input.id()? != id || record.revision == 0 {
        return Err("The shared context's saved identity changed.".into());
    }
    Ok(record)
}
pub(crate) fn save(db: &Connection, record: &Record) -> Result<()> {
    db.execute("INSERT INTO shared_contexts VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET record=excluded.record", params![record.id.to_string(),record.input.session,record.input.project.id,serde_json::to_string(record)?])?;
    db.execute(
        "INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'shared-context.saved',?3)",
        params![
            crate::journal::timestamp(),
            record.input.session,
            serde_json::json!({"id":record.id,"revision":record.revision,"phase":record.phase})
                .to_string()
        ],
    )?;
    Ok(())
}
fn ready(db: &Connection, session: &str, folder: &str) -> Result<Vec<Saved>> {
    let mut query=db.prepare("SELECT id FROM shared_contexts WHERE session=?1 AND folder=?2 AND json_extract(record,'$.phase')='ready' ORDER BY rowid")?;
    query
        .query_map(params![session, folder], |row| row.get::<_, String>(0))?
        .map(|id| {
            let record = read(db, Uuid::parse_str(&id?)?)?;
            let Some(Computed {
                outcome: Outcome::View(view),
                ..
            }) = record.computed
            else {
                return Err("The admitted shared context has no immutable view.".into());
            };
            Ok(Saved {
                input: record.input.projection,
                view: *view,
            })
        })
        .collect()
}
pub(crate) fn prepare(db: &mut Connection, request: &Request) -> Result<Preparation> {
    let tx = db.transaction()?;
    let job = crate::integration_store::read(&tx, request.job)?;
    let result = match plan(&tx, &job, request)? {
        shared_plan::Next::Ready(views) => Preparation::Ready(views),
        shared_plan::Next::Project(projection) => {
            let input = Input {
                graph: projection.freeze(&job.lineage)?,
                session: job.session_id,
                slug: job.slug,
                project: job.project,
                projection: *projection,
            };
            let id = input.id()?;
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM shared_contexts WHERE id=?1)",
                [id.to_string()],
                |row| row.get(0),
            )?;
            let record = if exists {
                let old = read(&tx, id)?;
                if old.input.projection != input.projection
                    || old.input.session != input.session
                    || old.input.slug != input.slug
                    || old.input.project.id != input.project.id
                    || old.input.project.root != input.project.root
                    || old.input.project.common_dir != input.project.common_dir
                {
                    return Err(
                        "The shared context now belongs to another source or repository.".into(),
                    );
                }
                old
            } else {
                let record = Record {
                    id,
                    revision: 1,
                    input,
                    phase: Phase::Pending,
                    solutions: vec![],
                    computed: None,
                };
                save(&tx, &record)?;
                record
            };
            Preparation::Step(Box::new(record))
        }
    };
    tx.commit()?;
    Ok(result)
}
pub(crate) fn plan(
    db: &Connection,
    job: &crate::integration::Job,
    request: &Request,
) -> Result<shared_plan::Next> {
    if job.id != request.job {
        return Err("The shared context belongs to another review.".into());
    }
    if job.revision != request.revision
        || !job.contributions.is_empty()
        || matches!(
            job.status,
            crate::integration::Status::Done
                | crate::integration::Status::Discarded
                | crate::integration::Status::Committing
                | crate::integration::Status::CommitBlocked
        )
    {
        return Err("Review the chosen paths before preparing their shared context.".into());
    }
    let state = job
        .path_state
        .as_ref()
        .ok_or("Missing shared dependency review.")?;
    if !state.pending.is_empty() || !state.mixed.is_empty() {
        return Err(
            "Choose paths without mixed published changes before preparing shared context.".into(),
        );
    }
    let parent = state
        .descendants
        .iter()
        .find(|child| child.track == request.descendant)
        .and_then(|child| {
            child
                .parents
                .iter()
                .find(|parent| parent.track == request.parent)
        })
        .ok_or("This review no longer has the affected shared dependency.")?;
    if parent.onto != parent.track
        || !job
            .path_review
            .shared_prefixes()?
            .get(&parent.track)
            .is_some_and(|through| parent.checkpoint.number <= through.number)
    {
        return Err("Realign the chosen parent before its descendant.".into());
    }
    let graph = &job.lineage;
    let source = graph.by_sha(&parent.track, &parent.checkpoint.sha)?;
    if source.inherited() != parent.checkpoint {
        return Err("The shared source checkpoint changed.".into());
    }
    let mut rewrites = vec![];
    for choice in job.path_review.choices.values() {
        let checkpoint = job
            .tracks
            .iter()
            .find(|track| track.id == choice.kept)
            .and_then(|track| track.checkpoint.as_ref())
            .ok_or("The chosen path has no frozen checkpoint.")?;
        let target = graph.by_sha(&choice.kept, &checkpoint.sha)?.clone();
        for (track, boundary) in &choice.retired {
            rewrites.push(Rewrite {
                track: track.clone(),
                shared_through: boundary.shared_through(),
                target: target.clone(),
            });
        }
    }
    let checkpoint = job
        .tracks
        .iter()
        .find(|track| track.id == request.descendant)
        .and_then(|track| track.checkpoint.as_ref())
        .ok_or("The shared consumer has no frozen checkpoint.")?;
    let consumer = graph.by_sha(&request.descendant, &checkpoint.sha)?;
    shared_plan::next(
        graph,
        source,
        &consumer.views,
        &rewrites,
        &ready(db, &job.session_id, &job.project.id)?,
    )
}
pub(crate) fn change(db: &mut Connection, change: Change) -> Result<Record> {
    let tx = db.transaction()?;
    let (id, revision) = match &change {
        Change::Begin { id, revision, .. } | Change::Admit { id, revision } => (*id, *revision),
    };
    let mut record = read(&tx, id)?;
    // A lost acknowledgement may replay exactly the immediately prior phase.
    match change {
        Change::Begin { computed, .. } => {
            let replay_revision = revision
                .checked_add(match record.phase {
                    Phase::Pinning => 1,
                    Phase::Ready | Phase::NeedsMerge => 2,
                    Phase::Pending => 0,
                })
                .ok_or("Shared context revision overflow.")?;
            if record.phase != Phase::Pending
                && record.revision == replay_revision
                && record.computed.as_ref() == Some(computed.as_ref())
            {
                tx.commit()?;
                return Ok(record);
            }
            if record.phase != Phase::Pending
                || record.revision != revision
                || record.computed.is_some()
            {
                return Err("The shared computation was superseded.".into());
            }
            computed.anchor.parse::<intercat_git::ObjectId>()?;
            match &computed.outcome {
                Outcome::View(view)
                    if view.track == record.input.projection.source.track
                        && view.source == record.input.projection.source.inherited()
                        && view.head == computed.anchor => {}
                Outcome::Conflict(conflict) if !conflict.paths.is_empty() => {
                    for tree in [&conflict.base, &conflict.target, &conflict.incoming] {
                        tree.parse::<intercat_git::ObjectId>()?;
                    }
                }
                _ => return Err("The computed shared context has another source.".into()),
            }
            record.computed = Some(*computed);
            record.phase = Phase::Pinning;
        }
        Change::Admit { .. } => {
            if matches!(record.phase, Phase::Ready | Phase::NeedsMerge)
                && record.revision
                    == revision
                        .checked_add(1)
                        .ok_or("Shared context revision overflow.")?
            {
                tx.commit()?;
                return Ok(record);
            }
            if record.phase != Phase::Pinning || record.revision != revision {
                return Err("The shared object admission was superseded.".into());
            }
            record.phase = match record
                .computed
                .as_ref()
                .ok_or("Missing shared computation.")?
                .outcome
            {
                Outcome::View(_) => Phase::Ready,
                Outcome::Conflict(_) => Phase::NeedsMerge,
            };
        }
    }
    record.revision = record
        .revision
        .checked_add(1)
        .ok_or("Shared context revision overflow.")?;
    save(&tx, &record)?;
    tx.commit()?;
    Ok(record)
}
