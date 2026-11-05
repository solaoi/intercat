use intercat_daemon::policy::{self, Context, Reason, Stops};
fn decide(request: &serde_json::Value, every: bool) -> Option<Reason> {
    policy::evaluate(
        request,
        &Context {
            stops: if every {
                Stops::EveryApproval
            } else {
                Stops::IrreversibleOnly
            },
            workspace: std::path::Path::new("/work"),
            recoverable_deletion: false,
            remote_hosts: &[],
        },
    )
    .review
}
use serde_json::json;
fn shell(argv: Vec<&str>) -> serde_json::Value {
    json!({"rawArgs":"{}","protectedWrite":false,"currentRequirementId":{"sourceIndex":0},"subject":{"kind":"shell","command":argv.join(" "),"stages":[{"argv":argv,"argvComplete":true}]}})
}

#[test]
fn a_final_sleep_is_waiting_but_sleep_before_more_work_is_scheduling() {
    let mut request = shell(vec!["/usr/bin/touch", "marker"]);
    request["subject"]["command"] = "/usr/bin/touch marker; /bin/sleep 60".into();
    request["subject"]["stages"] = json!([
        {"argv":["/usr/bin/touch","marker"],"argvComplete":true,"position":1,"totalStages":2},
        {"argv":["/bin/sleep","60"],"argvComplete":true,"position":2,"totalStages":2}
    ]);
    request["currentRequirementId"]["sourceIndex"] = json!(1);
    assert!(decide(&request, false).is_none());
    request["subject"]["command"] = "/bin/sleep 60; /usr/bin/touch marker".into();
    request["subject"]["stages"] = json!([
        {"argv":["/bin/sleep","60"],"argvComplete":true,"position":1,"totalStages":2},
        {"argv":["/usr/bin/touch","marker"],"argvComplete":true,"position":2,"totalStages":2}
    ]);
    request["currentRequirementId"]["sourceIndex"] = json!(0);
    assert_eq!(decide(&request, false).unwrap().kind, "schedule");
    assert_eq!(
        decide(
            &shell(vec!["/bin/sh", "-c", "sleep 60; touch marker"]),
            false
        )
        .unwrap()
        .kind,
        "schedule"
    );
    request["currentRequirementId"]["sourceIndex"] = json!(u64::MAX);
    assert!(decide(&request, false).is_some());
}
#[test]
fn policy_separates_regular_commands_from_actions_that_need_review() {
    assert!(decide(&shell(vec!["node", "--test"]), false).is_none());
    assert_eq!(
        decide(&shell(vec!["git", "push", "--force"]), false)
            .unwrap()
            .kind,
        "push"
    );
    assert_eq!(
        decide(&shell(vec!["git", "push"]), false).unwrap().kind,
        "push"
    );
    assert_eq!(
        decide(&shell(vec!["rm", "file"]), false).unwrap().kind,
        "delete"
    );
    assert_eq!(
        decide(&shell(vec!["git", "reset", "--hard"]), false)
            .unwrap()
            .kind,
        "delete"
    );
    let mut elevated = shell(vec!["node", "--test"]);
    elevated["rawArgs"] = json!("{\"sandbox_permissions\":\"require_escalated\"}");
    assert_eq!(decide(&elevated, false).unwrap().kind, "escalation");
    let mut protected = shell(vec!["node", "--test"]);
    protected["protectedWrite"] = true.into();
    assert_eq!(decide(&protected, false).unwrap().kind, "write");
    assert_eq!(
        decide(
            &json!({"subject":{"kind":"network","host":"example.invalid"}}),
            false
        )
        .unwrap()
        .kind,
        "send"
    );
    assert!(
        decide(
            &json!({"subject":{"kind":"network","host":"127.0.0.1"}}),
            false
        )
        .is_none()
    );
    assert_eq!(
        decide(&shell(vec!["node", "--test"]), true).unwrap().kind,
        "approval"
    );
}
#[test]
fn opaque_commands_and_unknown_subjects_are_not_silently_approved() {
    let value = json!({"subject":{"kind":"shell","command":"eval $COMMAND","stages":[]}});
    assert!(decide(&value, false).is_some());
    assert!(decide(&json!({"subject":{"kind":"new-tool"}}), false).is_some());
}

#[test]
fn datastore_deploy_send_and_hidden_work_match_the_known_patterns() {
    for (argv, kind) in [
        (vec!["git", "-C", "/work", "push", "origin"], "push"),
        (vec!["psql", "-c", "DROP TABLE users"], "migrate"),
        (
            vec!["sqlite3", "data.db", "UPDATE settings SET value=0"],
            "migrate",
        ),
        (
            vec![
                "mysql",
                "-e",
                "DELETE FROM users; SELECT * FROM a WHERE id=1",
            ],
            "migrate",
        ),
        (vec!["redis-cli", "FLUSHALL"], "migrate"),
        (vec!["xargs", "redis-cli", "DEL"], "migrate"),
        (vec!["mongosh", "--eval", "db.users.drop()"], "migrate"),
        (vec!["npx", "prisma", "migrate", "reset"], "migrate"),
        (vec!["rails", "db:reset"], "migrate"),
        (vec!["python3", "manage.py", "flush"], "migrate"),
        (vec!["docker", "compose", "down", "-v"], "migrate"),
        (vec!["docker", "volume", "rm", "data"], "migrate"),
        (vec!["npm", "run", "deploy"], "deploy"),
        (vec!["terraform", "apply"], "deploy"),
        (
            vec![
                "curl",
                "-XPOST",
                "https://example.invalid/api?secret=hidden",
            ],
            "send",
        ),
        (
            vec![
                "curl",
                "--data-binary",
                "@payload.json",
                "https://example.invalid",
            ],
            "send",
        ),
        (vec!["sendmail", "user@example.invalid"], "send"),
        (vec!["crontab", "jobs.txt"], "schedule"),
        (
            vec!["launchctl", "bootstrap", "gui/501", "job.plist"],
            "schedule",
        ),
        (vec!["nohup", "npm", "run", "dev"], "detach"),
        (vec!["bash", "-c", "redis-cli FLUSHDB"], "migrate"),
        (vec!["codex", "exec", "task"], "nested-agent"),
    ] {
        let request = shell(argv);
        let verdict = policy::evaluate(
            &request,
            &Context {
                stops: Stops::IrreversibleOnly,
                workspace: std::path::Path::new("/work"),
                recoverable_deletion: false,
                remote_hosts: &[],
            },
        );
        assert_eq!(
            verdict
                .review
                .as_ref()
                .unwrap_or_else(|| panic!("Missing review: {request}"))
                .kind,
            kind,
            "{request}"
        );
        if kind == "schedule" || kind == "detach" {
            assert!(!verdict.allow_once);
        }
        if let Some(impact) = verdict.impact {
            assert!(!impact.target.contains("secret=hidden"));
        }
    }
    for argv in [
        vec!["git", "log", "--grep", "push"],
        vec!["git", "show", "HEAD:push"],
        vec!["echo", "deploy"],
        vec!["echo", "migrate"],
        vec!["redis-cli", "GET", "key"],
        vec!["psql", "-c", "SELECT * FROM data"],
        vec!["sqlite3", "data.db", "DELETE FROM users WHERE id=1"],
        vec!["curl", "https://example.invalid"],
        vec!["docker", "compose", "down"],
        vec!["node", "--test"],
    ] {
        assert!(decide(&shell(argv.clone()), false).is_none(), "{argv:?}");
    }
    let mut stages = shell(vec!["echo", "first"]);
    stages["currentRequirementId"]["sourceIndex"] = json!(1);
    stages["subject"]["stages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"argv":["rm","file"],"argvComplete":true}));
    assert_eq!(decide(&stages, false).unwrap().kind, "delete");
}

#[test]
fn destinations_and_write_boundaries_do_not_trust_origin_display_text() {
    let hosts = vec!["git.example.invalid".into()];
    let context = Context {
        stops: Stops::IrreversibleOnly,
        workspace: std::path::Path::new("/work"),
        recoverable_deletion: false,
        remote_hosts: &hosts,
    };
    let network = |host| json!({"subject":{"kind":"network","host":host,"origin":{"kind":"shellCommand","command":"curl -X POST"}}});
    for host in [
        "git.example.invalid",
        "GIT.EXAMPLE.INVALID.",
        "registry.npmjs.org",
        "localhost",
    ] {
        assert!(policy::evaluate(&network(host), &context).review.is_none());
    }
    assert!(
        policy::evaluate(&network("git.example.invalid.attacker.invalid"), &context)
            .review
            .is_some()
    );
    let write = |path| json!({"subject":{"kind":"fileAccess","access":"write","path":path}});
    assert!(
        policy::evaluate(&write("/work/file.txt"), &context)
            .review
            .is_none()
    );
    assert_eq!(
        policy::evaluate(&write("/work-other/file.txt"), &context)
            .review
            .unwrap()
            .kind,
        "external-write"
    );
    assert_eq!(
        policy::evaluate(&write("../file.txt"), &context)
            .review
            .unwrap()
            .kind,
        "external-write"
    );
    assert_eq!(
        policy::evaluate(&write(".git/config"), &context)
            .review
            .unwrap()
            .kind,
        "write"
    );
    let mut elevated = shell(vec!["git", "push", "origin"]);
    elevated["rawArgs"] = json!("{\"sandbox_permissions\":\"require_escalated\"}");
    let verdict = policy::evaluate(&elevated, &context);
    assert_eq!(verdict.review.unwrap().kind, "escalation");
    assert_eq!(verdict.impact.unwrap().kind, "push");
}

#[tokio::test]
async fn stops_are_durable_and_each_policy_decision_uses_the_committed_setting()
-> intercat_daemon::Result<()> {
    use intercat_daemon::journal::Journal;
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let j = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    let mut request = shell(vec!["node", "--test"]);
    request["sessionId"] = json!("s");
    request["approvalId"] = json!("a");
    request["itemId"] = json!("i");
    assert!(
        j.evaluate_approval(
            "track".into(),
            "/work".into(),
            vec![],
            request.clone(),
            None
        )
        .await?
        .review
        .is_none()
    );
    let operation = uuid::Uuid::now_v7();
    j.set_stops(operation, Stops::EveryApproval).await?;
    assert_eq!(
        j.evaluate_approval(
            "track".into(),
            "/work".into(),
            vec![],
            request.clone(),
            None
        )
        .await?
        .review
        .unwrap()
        .kind,
        "approval"
    );
    assert!(
        j.set_stops(operation, Stops::IrreversibleOnly)
            .await
            .is_err()
    );
    let sequence = j.snapshot().await?["sequence"].clone();
    j.set_stops(operation, Stops::EveryApproval).await?;
    assert_eq!(j.snapshot().await?["sequence"], sequence);
    drop(j);
    let j = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    assert_eq!(j.snapshot().await?["stops"], "every-approval");
    j.set_stops(uuid::Uuid::now_v7(), Stops::IrreversibleOnly)
        .await?;
    assert!(
        j.evaluate_approval("track".into(), "/work".into(), vec![], request, None)
            .await?
            .review
            .is_none()
    );
    assert!(serde_json::from_value::<Stops>(json!("never")).is_err());
    Ok(())
}
