//! The group reserves every folder before any publication starts. Its observer
//! persists partial completion while the existing folder operations recover.
use super::{Action, Command, Running, Service, ServiceState, Status};
use crate::{
    Result,
    commit_group::{self, Change, Group},
};
use std::time::Duration;
use tokio::sync::watch;
use uuid::Uuid;

impl Service {
    pub async fn open_group(&self, request: commit_group::Open) -> Result<Group> {
        let mut state = self.state.lock().await;
        self.reap_finished(&mut state).await?;
        let changed = self
            .journal
            .change_commit_group(Change::Open(request.clone()))
            .await?;
        let id = changed.group.id;
        for job in changed.jobs {
            let job = self.advance(&mut state, job).await?;
            if job.status == Status::Open
                && job.plan.as_ref().is_some_and(|plan| plan.tree.is_some())
                || job.status == Status::MergeConflict
                    && job.plan.as_ref().is_some_and(|plan| {
                        plan.undo_conflict.is_some()
                            || plan
                                .rows
                                .iter()
                                .any(|row| row.state == super::RowState::Conflict)
                    })
            {
                let op_id = Uuid::new_v5(&request.op_id, format!("prepare/{}", job.id).as_bytes());
                if self.journal.operation(op_id).await?.is_none() {
                    let job = self
                        .journal
                        .integration_command(Command {
                            op_id,
                            action: Action::Prepare {
                                id: job.id,
                                revision: job.revision,
                            },
                        })
                        .await?;
                    self.advance(&mut state, job).await?;
                }
            }
        }
        self.watch_groups(&mut state).await?;
        Ok(self
            .journal
            .change_commit_group(Change::Reconcile(id))
            .await?
            .group)
    }
    pub async fn commit_group(&self, request: commit_group::Commit) -> Result<Group> {
        let mut state = self.state.lock().await;
        self.reap_finished(&mut state).await?;
        let changed = self
            .journal
            .change_commit_group(Change::Commit(request))
            .await?;
        let id = changed.group.id;
        for job in changed.jobs {
            self.advance(&mut state, job).await?;
        }
        self.watch_groups(&mut state).await?;
        Ok(self
            .journal
            .change_commit_group(Change::Reconcile(id))
            .await?
            .group)
    }
    pub async fn discard_group(&self, request: commit_group::Discard) -> Result<Group> {
        let mut state = self.state.lock().await;
        self.reap_finished(&mut state).await?;
        let changed = self
            .journal
            .change_commit_group(Change::Discard(request))
            .await?;
        for job in changed.jobs {
            self.advance(&mut state, job).await?;
        }
        Ok(changed.group)
    }
    pub(super) async fn reconcile_groups(&self) -> Result<Vec<Group>> {
        let mut groups = Vec::new();
        for group in self.journal.active_commit_groups().await? {
            groups.push(
                self.journal
                    .change_commit_group(Change::Reconcile(group.id))
                    .await?
                    .group,
            );
        }
        Ok(groups)
    }
    pub(super) async fn watch_groups(&self, state: &mut ServiceState) -> Result<()> {
        for group in self.reconcile_groups().await? {
            if matches!(
                group.status,
                commit_group::Status::Done | commit_group::Status::Discarded
            ) {
                continue;
            }
            if let std::collections::btree_map::Entry::Vacant(slot) = state.running.entry(group.id)
            {
                let (cancel, mut stopping) = watch::channel(false);
                let journal = self.journal.clone();
                let id = group.id;
                let task = tokio::spawn(async move {
                    let mut changes = journal.subscribe();
                    loop {
                        changes.borrow_and_update();
                        if *stopping.borrow() {
                            break;
                        }
                        if let Ok(changed) =
                            journal.change_commit_group(Change::Reconcile(id)).await
                            && matches!(
                                changed.group.status,
                                commit_group::Status::Done | commit_group::Status::Discarded
                            )
                        {
                            break;
                        }
                        tokio::select! {_=stopping.changed()=>{},_=changes.changed()=>{},_=tokio::time::sleep(Duration::from_secs(1))=>{}}
                    }
                });
                slot.insert(Running {
                    publishing: false,
                    revision: group.revision,
                    cancel,
                    task,
                });
            }
        }
        Ok(())
    }
}
