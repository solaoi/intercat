//! Compute outside SQLite, record the result, protect its exact objects, then
//! admit it. Reopening during pinning reuses the saved result and input.
use crate::{
    Result,
    journal::Journal,
    shared_context::Outcome,
    shared_context_store::{Change, Computed, Phase, Preparation, Record, Request},
};
use intercat_git::Repository;

pub(crate) fn source_ref(slug: &str, sha: &str) -> String {
    format!("refs/intercat/{slug}/shared-sources/{sha}")
}
#[derive(Debug)]
pub(crate) struct Required;
impl std::fmt::Display for Required {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Shared changes need a merge before realignment can continue.")
    }
}
impl std::error::Error for Required {}

fn repository(record: &Record) -> Result<Repository> {
    let repo = Repository::open(&record.input.project.root)?;
    if repo.common_directory()? != record.input.project.common_dir
        || !repo.owns_session(&record.input.slug, &record.input.session)?
    {
        return Err("The shared context's session repository changed.".into());
    }
    Ok(repo)
}
pub(crate) fn compute(record: &Record) -> Result<Computed> {
    let repo = repository(record)?;
    let outcome = crate::shared_context::project(
        &repo,
        &record.input.graph,
        &record.input.projection,
        &record.solutions,
    )?;
    let anchor = match &outcome {
        Outcome::View(view) => view.head.clone(),
        Outcome::Conflict(conflict) => repo
            .context_commit(
                conflict.target.parse()?,
                record.input.projection.source.sha.parse()?,
                &format!(
                    "Intercat shared merge source\n\n{}\n",
                    serde_json::to_string(&(record.id, &record.solutions, conflict))?
                ),
            )?
            .to_string(),
    };
    Ok(Computed { outcome, anchor })
}
pub(crate) fn pin(record: &Record) -> Result<()> {
    let repo = repository(record)?;
    let computed = record
        .computed
        .as_ref()
        .ok_or("The shared step has no computed objects.")?;
    if compute(record)? != *computed {
        return Err("The shared computation no longer reproduces its saved objects.".into());
    }
    let mut roots = record.input.graph.roots()?;
    crate::lineage::view::roots(&record.input.projection.views, &mut roots)?;
    if let Some(previous) = &record.input.projection.previous {
        crate::lineage::view::roots(&vec![previous.clone()], &mut roots)?;
    }
    roots.push(computed.anchor.parse()?);
    match &computed.outcome {
        Outcome::View(view) => crate::lineage::view::roots(&vec![*view.clone()], &mut roots)?,
        Outcome::Conflict(conflict) => {
            for tree in [&conflict.base, &conflict.target, &conflict.incoming] {
                roots.push(tree.parse()?);
            }
        }
    }
    roots.sort();
    roots.dedup();
    repo.sync_objects(&roots)?;
    for sha in roots {
        let reference = format!(
            "refs/intercat/{}/shared-contexts/{}/objects/{sha}",
            record.input.slug, record.id
        );
        repo.compare_and_swap(&reference, None, sha)?;
        repo.sync_ref(&reference)?;
    }
    let source = record.input.projection.source.sha.parse()?;
    let reference = source_ref(&record.input.slug, &record.input.projection.source.sha);
    repo.compare_and_swap(&reference, None, source)?;
    repo.sync_ref(&reference)?;
    Ok(())
}
pub(crate) async fn admit(journal: &Journal, id: uuid::Uuid) -> Result<Record> {
    let record = journal.shared_context(id).await?;
    if matches!(record.phase, Phase::Ready | Phase::NeedsMerge) {
        return Ok(record);
    }
    if record.phase != Phase::Pinning {
        return Err("Compute the shared input before admitting it.".into());
    }
    let input = record.clone();
    tokio::task::spawn_blocking(move || pin(&input)).await??;
    journal
        .change_shared_context(Change::Admit {
            id: record.id,
            revision: record.revision,
        })
        .await
}
pub(crate) async fn advance(journal: &Journal, request: Request) -> Result<Preparation> {
    loop {
        let mut record = match journal.prepare_shared_context(request.clone()).await? {
            Preparation::Step(record) => record,
            ready => return Ok(ready),
        };
        match record.phase {
            Phase::Pending => {
                let input = record.clone();
                let computed = tokio::task::spawn_blocking(move || compute(&input)).await??;
                record = Box::new(
                    journal
                        .change_shared_context(Change::Begin {
                            id: record.id,
                            revision: record.revision,
                            computed: Box::new(computed),
                        })
                        .await?,
                );
            }
            Phase::Pinning => {}
            Phase::NeedsMerge => return Ok(Preparation::Step(record)),
            Phase::Ready => {
                return Err("The shared planner requested an already admitted step.".into());
            }
        }
        let admitted = admit(journal, record.id).await?;
        if admitted.phase == Phase::NeedsMerge {
            return Ok(Preparation::Step(Box::new(admitted)));
        }
    }
}
