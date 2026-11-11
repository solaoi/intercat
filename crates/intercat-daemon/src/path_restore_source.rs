//! Capture one retired path and its exact published target in the journal's
//! serialized read. Git verification and file synthesis run outside the writer.
use crate::{
    Result,
    integration::Status,
    lineage::{Ancestors, Basis, Frozen},
    path_retirement::{Entry, Kind, Phase},
};
use intercat_git::Repository;
use rusqlite::{Connection, OptionalExtension};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Context {
    pub entry: Entry,
    pub common_directory: PathBuf,
    pub target_ref: String,
    pub target: String,
    pub coverage: Ancestors,
    pub prefix: Ancestors,
    pub basis: Basis,
    pub graph: Frozen,
    pub cancelled: Vec<crate::integration_record::Record>,
}
impl Context {
    pub fn verify_repository(&self) -> Result<Repository> {
        let repo = Repository::open(&self.entry.project_root)?;
        let saved = self
            .entry
            .saved
            .as_ref()
            .ok_or("The retired path has no saved boundary.")?;
        if repo.common_directory()? != self.common_directory
            || !repo.owns_session(&self.entry.slug, &self.entry.session)?
            || repo.reference_target(&self.target_ref)? != Some(self.target.parse()?)
            || repo.reference_target(&saved.reference)? != Some(saved.sha.parse()?)
        {
            return Err("The Restore repository, target or saved path changed.".into());
        }
        repo.commit_details(self.target.parse()?)?;
        repo.commit_details(saved.sha.parse()?)?;
        Ok(repo)
    }
}
pub(crate) fn capture(db: &Connection, id: Uuid) -> Result<Context> {
    let entry = crate::path_retirement::read(db, id)?;
    crate::continuation::ensure_writer(db, &entry.track)?;
    if entry.phase != Phase::Aside
        || entry.kind != Kind::Aside
        || entry.saved.is_none()
        || !crate::path_retirement::holds(db, &entry.track)?
    {
        return Err("Restore needs a saved, set-aside path.".into());
    }
    let job = crate::integration_store::read(db, entry.job)?;
    if job.status != Status::Done
        || job
            .publication
            .as_ref()
            .is_none_or(|publication| publication.operation != entry.publication)
        || entry.project_root != job.project.root
        || entry.slug != job.slug
        || crate::folders::track(db, &entry.track)?.id() != entry.folder
    {
        return Err("The path's replacement or folder changed before Restore.".into());
    }
    let published: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND kind='integration.publish' AND state='complete')",
        [entry.publication.to_string()],
        |row| row.get(0),
    )?;
    if !published {
        return Err("The retired path's publication is not complete.".into());
    }
    let raw: String = db.query_row(
        "SELECT record FROM workspaces WHERE track_id=?1",
        [&entry.track],
        |row| row.get(0),
    )?;
    if serde_json::from_str::<serde_json::Value>(&raw)? != serde_json::to_value(&entry.workspace)? {
        return Err("The set-aside path's original workspace changed.".into());
    }
    let plan = job
        .plan
        .as_ref()
        .ok_or("Missing replacement publication plan.")?;
    let published: Option<(String, String)> = db
        .query_row(
            "SELECT head,coverage FROM published_lineage WHERE reference=?1",
            [&plan.target_ref],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    // An Aside always follows a successful publication. Missing coverage is
    // not an initial-branch shortcut here.
    let (target, coverage) = published.ok_or("The Restore target has no published coverage.")?;
    let prefix = job
        .path_review
        .families
        .iter()
        .find(|family| family.members.iter().any(|member| member.id == entry.track))
        .ok_or("The restored path has no saved family.")?
        .prefix(&entry.track)?;
    let cancelled = crate::replacement_undo_store::restore_records(db, &entry, &job, &prefix)?;
    let coverage: Ancestors = serde_json::from_str(&coverage)?;
    let tracks = coverage
        .keys()
        .chain(prefix.keys())
        .cloned()
        .chain(std::iter::once(entry.track.clone()))
        .collect();
    let graph = crate::lineage_store::freeze(db)?.for_tracks(tracks);
    Ok(Context {
        common_directory: job.project.common_dir,
        target_ref: plan.target_ref.clone(),
        target,
        coverage,
        prefix,
        basis: crate::lineage_store::basis(db, &entry.track)?,
        graph,
        cancelled,
        entry,
    })
}
