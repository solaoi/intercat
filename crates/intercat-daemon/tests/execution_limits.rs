use intercat_daemon::{execution_limits::Change, journal::Journal, secrets::Key};
use uuid::Uuid;

#[tokio::test]
async fn limit_is_durable_revision_checked_and_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("events.sqlite");
    let journal = Journal::open(&path, Key::new([7; 32])).await.unwrap();
    assert_eq!(
        journal.execution_limits().await.unwrap().parallel_workers,
        4
    );
    let request = Change {
        op_id: Uuid::now_v7(),
        revision: 0,
        parallel_workers: 1,
        planner_reviewer_turns: 1,
    };
    let saved = journal
        .change_execution_limits(request.clone())
        .await
        .unwrap();
    assert_eq!(saved.revision, 1);
    assert_eq!(saved.planner_reviewer_turns, 1);
    assert_eq!(
        journal
            .change_execution_limits(request.clone())
            .await
            .unwrap(),
        saved
    );
    let mut stale = request.clone();
    stale.op_id = Uuid::now_v7();
    stale.parallel_workers = 3;
    assert!(
        journal
            .change_execution_limits(stale)
            .await
            .unwrap_err()
            .to_string()
            .contains("another window")
    );
    let mut reused = request.clone();
    reused.parallel_workers = 2;
    assert!(journal.change_execution_limits(reused).await.is_err());
    let invalid = Change {
        op_id: Uuid::now_v7(),
        revision: 1,
        parallel_workers: 0,
        planner_reviewer_turns: 1,
    };
    assert!(journal.change_execution_limits(invalid).await.is_err());
    drop(journal);
    let reopened = Journal::open(path, Key::new([7; 32])).await.unwrap();
    assert_eq!(reopened.execution_limits().await.unwrap(), saved);
}
