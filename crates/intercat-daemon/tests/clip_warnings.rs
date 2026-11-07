use intercat_daemon::{
    Result,
    clip_summaries::{Finish, Summary},
    clock::Stamp,
    decision::{Decision, Kind, Outcome, Target},
    journal::Journal,
    secrets::Key,
    timeline::Window,
};
use serde_json::{Value, json};
use uuid::Uuid;

struct Fixture {
    root: tempfile::TempDir,
    journal: Journal,
    tracks: Vec<String>,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        for index in 0..3 {
            journal
                .stage_utterance(
                    Uuid::now_v7(),
                    format!("Dummy task {index}"),
                    Target::New,
                    None,
                    vec![],
                )
                .await?;
            let input = journal.claim_input().await?.unwrap();
            journal
                .apply_input(
                    input.id,
                    input.revision,
                    Outcome::Classified(Decision {
                        kind: Kind::Fork,
                        track_id: None,
                        boundary: Some("now".into()),
                        command: None,
                        confidence: 1.0,
                        inference_ms: 0.0,
                        timed_out: false,
                        scores: json!({}),
                    }),
                )
                .await?;
        }
        let tracks = journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|track| track["id"].as_str().unwrap().to_owned())
            .collect();
        Ok(Self {
            root,
            journal,
            tracks,
        })
    }
    async fn put(&self, track: usize, time: i64, item: Value) -> Result<()> {
        self.journal
            .try_msp(
                self.tracks[track].clone(),
                format!("session-{track}"),
                Stamp {
                    epoch_ms: time + 1000,
                    mono_ms: time,
                    boot_id: "dummy".into(),
                    timeline_ms: time + 1000,
                },
                item,
                None,
                None,
            )?
            .await??;
        Ok(())
    }
    async fn step(&self, track: usize, turn: &str, time: i64) -> Result<String> {
        self.message(track, turn, 1, time).await?;
        self.put(
            track,
            time + 100,
            json!({"method":"turn/completed","params":{
            "sessionId":format!("session-{track}"),"turnId":turn,"terminal":"completed"}}),
        )
        .await?;
        let ticket = self.journal.claim_clip_summary().await?.unwrap();
        let id = ticket.clip.clone();
        self.journal
            .finish_clip_summary(
                ticket,
                Finish::Complete(Summary {
                    title: "Unique initial title".into(),
                    summary: "A dummy step completed.".into(),
                }),
            )
            .await?;
        Ok(id)
    }
    async fn message(&self, track: usize, turn: &str, revision: i64, time: i64) -> Result<()> {
        self.put(
            track,
            time,
            json!({"method":"item/updated","params":{
            "sessionId":format!("session-{track}"),"item":{"itemId":format!("answer-{turn}"),
                "turnId":turn,"kind":"agentMessage","revision":revision,"status":"completed",
                "text":format!("Dummy step {turn}, version {revision}.")}}}),
        )
        .await
    }
    async fn title(
        &self,
        track: usize,
        turn: &str,
        revision: i64,
        time: i64,
        title: &str,
    ) -> Result<String> {
        self.message(track, turn, revision, time).await?;
        let ticket = self.journal.claim_clip_summary().await?.unwrap();
        let id = ticket.clip.clone();
        self.journal
            .finish_clip_summary(
                ticket,
                Finish::Complete(Summary {
                    title: title.into(),
                    summary: "A dummy step completed.".into(),
                }),
            )
            .await?;
        Ok(id)
    }
    async fn window(&self, since: Option<i64>) -> Result<Value> {
        self.journal
            .timeline_window(Window {
                tracks: self.tracks.clone(),
                start: 0,
                end: i64::MAX,
                since,
            })
            .await
    }
}
fn clip<'a>(window: &'a Value, id: &str) -> &'a Value {
    window["clips"]
        .as_array()
        .unwrap()
        .iter()
        .find(|clip| clip["id"] == id)
        .unwrap()
}
fn warned(clip: &Value) -> bool {
    !clip["warnings"].as_array().unwrap().is_empty()
}

#[tokio::test]
async fn normalized_titles_mark_both_steps_without_invalidating_the_peers_summary() -> Result<()> {
    let f = Fixture::new().await?;
    let a = f.step(0, "a", 100).await?;
    f.title(0, "a", 2, 300, "Cache retry").await?;
    let b = f.step(1, "b", 500).await?;
    let before = f.window(None).await?;
    assert!(!warned(clip(&before, &a)));
    f.title(1, "b", 2, 700, " ＣＡＣＨＥ　retry ").await?;
    let delta = f.window(before["revision"].as_i64()).await?;
    assert!(warned(clip(&delta, &a)));
    assert!(warned(clip(&delta, &b)));
    assert_eq!(clip(&delta, &a)["title"], "Cache retry");
    assert!(
        clip(&delta, &a)["warnings"][0]["text"]
            .as_str()
            .unwrap()
            .contains("B:1")
    );
    assert!(
        clip(&delta, &b)["warnings"][0]["text"]
            .as_str()
            .unwrap()
            .contains("A:1")
    );
    assert!(f.journal.claim_clip_summary().await?.is_none());
    assert_eq!(
        f.journal.clip_detail(a.clone()).await?["clip"]["warnings"],
        clip(&delta, &a)["warnings"]
    );
    let c = f.step(2, "c", 900).await?;
    f.title(2, "c", 2, 1100, "cache retry").await?;
    let grouped = f.window(None).await?;
    assert!(
        clip(&grouped, &a)["warnings"][0]["text"]
            .as_str()
            .unwrap()
            .contains("B:1 and 1 other step")
    );
    assert!(warned(clip(&grouped, &c)));
    Ok(())
}

#[tokio::test]
async fn late_correction_clears_both_sides_and_stale_summary_cannot_restore_warning() -> Result<()>
{
    let f = Fixture::new().await?;
    let a = f.step(0, "a", 100).await?;
    let b = f.step(1, "b", 500).await?;
    let before = f.window(None).await?;
    assert!(warned(clip(&before, &a)));
    f.message(1, "b", 2, 700).await?;
    let outdated = f.journal.claim_clip_summary().await?.unwrap();
    let cleared = f.window(before["revision"].as_i64()).await?;
    assert!(!warned(clip(&cleared, &a)));
    assert!(!warned(clip(&cleared, &b)));
    assert_eq!(clip(&cleared, &a)["title"], "Unique initial title");
    f.message(1, "b", 3, 800).await?;
    f.journal
        .finish_clip_summary(
            outdated,
            Finish::Complete(Summary {
                title: "Unique initial title".into(),
                summary: "Outdated dummy summary".into(),
            }),
        )
        .await?;
    let newest = f.journal.claim_clip_summary().await?.unwrap();
    assert_eq!(newest.clip, b);
    f.journal
        .finish_clip_summary(
            newest,
            Finish::Complete(Summary {
                title: "Different work".into(),
                summary: "A corrected dummy step.".into(),
            }),
        )
        .await?;
    let after = f.window(None).await?;
    assert!(!warned(clip(&after, &a)));
    assert!(!warned(clip(&after, &b)));
    assert!(f.journal.claim_clip_summary().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn window_is_inclusive_at_ten_minutes_and_ignores_same_track() -> Result<()> {
    let f = Fixture::new().await?;
    let a = f.step(0, "a", 100).await?;
    let b = f.step(1, "b", 600_100).await?;
    let c = f.step(2, "c", 1_200_101).await?;
    let same = f.step(2, "c-again", 1_200_200).await?;
    let window = f.window(None).await?;
    assert!(warned(clip(&window, &a)));
    assert!(warned(clip(&window, &b)));
    assert!(!warned(clip(&window, &c)));
    assert!(!warned(clip(&window, &same)));
    Ok(())
}

#[tokio::test]
async fn warnings_survive_restart_and_removing_a_peer_refreshes_the_survivor() -> Result<()> {
    let f = Fixture::new().await?;
    let a = f.step(0, "a", 100).await?;
    let b = f.step(1, "b", 500).await?;
    let before = f.window(None).await?;
    let Fixture {
        root,
        journal,
        tracks,
    } = f;
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let f = Fixture {
        root,
        journal,
        tracks,
    };
    let restarted = f.window(None).await?;
    assert_eq!(
        clip(&before, &a)["warnings"],
        clip(&restarted, &a)["warnings"]
    );
    assert!(warned(clip(&restarted, &b)));
    // Only this test's generated database is opened, with no worker or service running.
    let db = rusqlite::Connection::open(f.root.path().join("db"))?;
    db.pragma_update(None, "foreign_keys", true)?;
    db.execute("DELETE FROM timeline_clips WHERE id=?1", [&b])?;
    drop(db);
    let deleted = f.window(restarted["revision"].as_i64()).await?;
    assert!(!warned(clip(&deleted, &a)));
    assert!(deleted["removed"].as_array().unwrap().contains(&json!(b)));
    assert!(f.journal.claim_clip_summary().await?.is_none());
    Ok(())
}
