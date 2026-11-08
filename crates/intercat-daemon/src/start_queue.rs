//! Facts that settle an unexpectedly queued start. An unqueue acknowledgement
//! alone is never a terminal event, and unrelated turn IDs cannot settle it.
use crate::Result;
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Reclaimed,
    Running,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Observed {
    Pending,
    Running,
    Removed,
}

pub(crate) struct QueuedStart {
    pub start: Uuid,
    pub turn: Uuid,
    pub command: Uuid,
    session: String,
    logged: bool,
    accepted: Option<bool>,
    observed: Observed,
}
impl QueuedStart {
    pub fn new(session: String, start: Uuid, turn: Uuid, command: Uuid) -> Self {
        Self {
            session,
            start,
            turn,
            command,
            logged: false,
            accepted: None,
            observed: Observed::Pending,
        }
    }
    pub fn logged(&mut self) {
        self.logged = true;
    }
    pub fn reply(&mut self, frame: &Value) -> Result<()> {
        if frame["error"].is_object() {
            let error: muse_code_sdk::msp::ErrorObject =
                serde_json::from_value(frame["error"].clone())?;
            let error = muse_code_sdk::errors::MspError::try_from(error)?;
            if !error.matches(muse_code_sdk::errors::ErrorCode::CommandRejected)
                || error.data().reason.as_deref() != Some("run_active")
                || error.data().command_id.as_deref() != Some(&self.command.to_string())
            {
                return Err(error.into());
            }
            self.accepted = Some(false);
        } else {
            let result = &frame["result"];
            if result["commandId"] != self.command.to_string()
                || result["turnId"] != self.turn.to_string()
                || result["status"] != "accepted"
            {
                return Err("The host did not confirm the exact queued-turn reclaim.".into());
            }
            self.accepted = Some(true);
        }
        self.outcome()?;
        Ok(())
    }
    pub fn observe(&mut self, method: &str, params: &Value) -> Result<()> {
        if params["sessionId"] != self.session || params["turnId"] != self.turn.to_string() {
            return Ok(());
        }
        let next = match method {
            "turn/started" | "turn/unqueued" => {
                if params["commandId"] != self.start.to_string() {
                    return Err("The queued turn changed its start command.".into());
                }
                if method == "turn/started" {
                    Observed::Running
                } else {
                    Observed::Removed
                }
            }
            "turn/completed" => Observed::Running,
            _ => return Ok(()),
        };
        if self.observed != Observed::Pending && self.observed != next {
            return Err("The host both ran and reclaimed the same queued turn.".into());
        }
        self.observed = next;
        self.outcome()?;
        Ok(())
    }
    pub fn outcome(&self) -> Result<Option<Outcome>> {
        if self.observed == Observed::Running && self.accepted == Some(true) {
            return Err("An accepted queued-turn reclaim unexpectedly ran.".into());
        }
        if !self.logged {
            return Ok(None);
        }
        Ok(match (self.observed, self.accepted) {
            (Observed::Removed, _) => Some(Outcome::Reclaimed),
            (Observed::Running, Some(false)) => Some(Outcome::Running),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn fixture() -> QueuedStart {
        QueuedStart::new(
            "session".into(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        )
    }
    fn event(q: &QueuedStart) -> Value {
        json!({"sessionId":"session","turnId":q.turn,"commandId":q.start})
    }
    #[test]
    fn reclaim_needs_the_matching_event_and_saved_intent_in_either_order() -> Result<()> {
        for event_first in [false, true] {
            let mut q = fixture();
            let event = event(&q);
            if event_first {
                q.observe("turn/unqueued", &event)?;
            }
            q.reply(
                &json!({"result":{"commandId":q.command,"turnId":q.turn,"status":"accepted"}}),
            )?;
            assert_eq!(q.outcome()?, None);
            q.logged();
            if !event_first {
                assert_eq!(q.outcome()?, None);
                q.observe("turn/unqueued", &event)?;
            }
            assert_eq!(q.outcome()?, Some(Outcome::Reclaimed));
            q.observe("turn/unqueued", &event)?;
            assert_eq!(q.outcome()?, Some(Outcome::Reclaimed));
        }
        Ok(())
    }
    #[test]
    fn a_refused_reclaim_needs_a_running_fact_and_never_releases_as_removed() -> Result<()> {
        let mut q = fixture();
        q.logged();
        q.reply(&json!({"error":{"code":-32030,"message":"Already running","data":{"kind":"commandRejected","commandId":q.command,"reason":"run_active"}}}))?;
        assert_eq!(q.outcome()?, None);
        q.observe("turn/started", &event(&q))?;
        assert_eq!(q.outcome()?, Some(Outcome::Running));
        Ok(())
    }
    #[test]
    fn mismatched_and_contradictory_facts_do_not_release_a_slot() -> Result<()> {
        let mut q = fixture();
        q.logged();
        q.observe(
            "turn/unqueued",
            &json!({"sessionId":"other","turnId":q.turn,"commandId":q.start}),
        )?;
        assert_eq!(q.outcome()?, None);
        assert!(
            q.observe(
                "turn/unqueued",
                &json!({"sessionId":"session","turnId":q.turn,"commandId":Uuid::now_v7()})
            )
            .is_err()
        );
        q.reply(&json!({"result":{"commandId":q.command,"turnId":q.turn,"status":"accepted"}}))?;
        assert!(q.observe("turn/started", &event(&q)).is_err());
        Ok(())
    }
}
