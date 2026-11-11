use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Hold {
    Stopping,
    Boundary,
    Log,
    Output,
    Editor,
    Reconciling,
    Reconnecting,
    Approval,
    Auth,
    Report,
    Review,
    Redirect,
    Realign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartPhase {
    Persisting,
    Sent,
}

#[derive(Debug, Clone, Copy)]
struct Reservation {
    id: Uuid,
    phase: StartPhase,
    stopped: bool,
    reply: bool,
}

/// Execution facts, not the UI's working/stopped label, decide whether to dispatch.
#[derive(Debug, Default)]
pub struct Run {
    active_turn: Option<Uuid>,
    reservation: Option<Reservation>,
    checkpoint_pending: Option<Uuid>,
    reconciled: bool,
    questions: BTreeSet<Uuid>,
    holds: Vec<Hold>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    PersistStart(Uuid),
    SendStart(Uuid),
    Superseded(Uuid),
    Interrupt(Uuid),
    Checkpoint(Uuid),
}

impl Run {
    pub fn new() -> Self {
        Self {
            reconciled: true,
            ..Self::default()
        }
    }
    pub fn active_turn(&self) -> Option<Uuid> {
        self.active_turn
    }
    pub fn set_hold(&mut self, hold: Hold, enabled: bool) {
        self.holds.retain(|h| *h != hold);
        if enabled {
            self.holds.push(hold);
        }
    }
    pub fn question(&mut self, id: Uuid) {
        self.questions.insert(id);
    }
    pub fn answer(&mut self, id: Uuid) {
        self.questions.remove(&id);
    }
    pub fn can_start(&self) -> bool {
        self.can_start_as(false)
    }
    pub fn can_reply(&self) -> bool {
        self.can_start_as(true)
    }
    fn can_start_as(&self, reply: bool) -> bool {
        self.reconciled
            && self.active_turn.is_none()
            && self.reservation.is_none()
            && self.checkpoint_pending.is_none()
            && (reply || self.questions.is_empty())
            && self.holds.iter().all(|hold| reply && *hold == Hold::Report)
    }
    pub fn request_start(&mut self, id: Uuid) -> Option<Effect> {
        self.request_start_as(id, false)
    }
    pub fn request_reply_start(&mut self, id: Uuid) -> Option<Effect> {
        self.request_start_as(id, true)
    }
    fn request_start_as(&mut self, id: Uuid, reply: bool) -> Option<Effect> {
        if !self.can_start_as(reply) {
            return None;
        }
        self.reservation = Some(Reservation {
            id,
            phase: StartPhase::Persisting,
            stopped: false,
            reply,
        });
        Some(Effect::PersistStart(id))
    }
    pub fn start_persisted(&mut self, id: Uuid) -> Option<Effect> {
        let reservation = self.reservation.as_mut()?;
        if reservation.id != id || reservation.phase != StartPhase::Persisting {
            return None;
        }
        // Recheck after the I/O: Stop, a question or a hold may have arrived meanwhile.
        if reservation.stopped
            || self
                .holds
                .iter()
                .any(|hold| !reservation.reply || *hold != Hold::Report)
            || (!reservation.reply && !self.questions.is_empty())
            || !self.reconciled
            || self.checkpoint_pending.is_some()
            || self.active_turn.is_some()
        {
            self.reservation = None;
            return Some(Effect::Superseded(id));
        }
        reservation.phase = StartPhase::Sent;
        Some(Effect::SendStart(id))
    }
    pub fn start_failed(&mut self, id: Uuid) {
        if self.reservation.is_some_and(|r| r.id == id) {
            self.reservation = None;
        }
    }
    pub fn start_acknowledged(&mut self, id: Uuid, turn: Uuid) -> Option<Effect> {
        let reservation = self.reservation?;
        if reservation.id != id || reservation.phase != StartPhase::Sent {
            return None;
        }
        self.reservation = None;
        self.active_turn = Some(turn);
        if reservation.stopped {
            Some(Effect::Interrupt(turn))
        } else {
            None
        }
    }
    pub fn stop(&mut self) -> Option<Effect> {
        self.set_hold(Hold::Stopping, true);
        if let Some(reservation) = self.reservation.as_mut() {
            reservation.stopped = true;
        }
        self.active_turn.map(Effect::Interrupt)
    }
    pub fn completed(&mut self, turn: Uuid) -> Option<Effect> {
        if self.active_turn != Some(turn) {
            return None;
        }
        self.active_turn = None;
        self.checkpoint_pending = Some(turn);
        Some(Effect::Checkpoint(turn))
    }
    pub fn checkpoint_saved(&mut self, turn: Uuid) {
        if self.checkpoint_pending == Some(turn) {
            self.checkpoint_pending = None;
        }
    }
    pub fn disconnected(&mut self) {
        self.reconciled = false;
    }
    pub fn reconciled(&mut self, active_turn: Option<Uuid>) {
        self.active_turn = active_turn;
        self.reconciled = true;
    }
}
