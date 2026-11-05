//! Bounded, ephemeral display text. Raw tails never enter snapshots or the log.
use ring::hmac;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

const MAX_ITEMS: usize = 16;
const MAX_TEXT: usize = 64 * 1024;
const MAX_TAIL: usize = 64 * 1024;

/// A saved fingerprint needs the original byte length, never the value.
#[derive(Clone)]
pub struct Fingerprint {
    pub name: String,
    pub byte_len: usize,
    pub tag: [u8; 32],
}

#[derive(Clone, Default)]
pub struct Mask {
    key: Option<hmac::Key>,
    fingerprints: Vec<Fingerprint>,
    max_len: usize,
}
type Covered = (usize, usize, String);
struct Released {
    text: String,
    used: usize,
    covered: Vec<Covered>,
    masking: bool,
}

impl Mask {
    pub fn redact(&self, text: &str) -> String {
        self.redact_text(text).0
    }
    fn redact_text(&self, text: &str) -> (String, bool) {
        if self.fingerprints.is_empty() {
            return (text.into(), false);
        }
        let (mut tail, mut covered, mut masking) = (String::new(), Vec::new(), false);
        let mut output = String::new();
        let mut total = 0usize;
        let mut remaining = text;
        while !remaining.is_empty() {
            let mut end = remaining.len().min(MAX_TEXT);
            while !remaining.is_char_boundary(end) {
                end -= 1;
            }
            tail.push_str(&remaining[..end]);
            remaining = &remaining[end..];
            let released = self.release(&tail, remaining.is_empty(), &covered, masking);
            tail.drain(..released.used);
            covered = released.covered;
            masking = released.masking;
            total = total.saturating_add(released.text.len());
            output.push_str(&released.text);
            if output.len() > 1024 * 1024 {
                // A cut can bisect an as-yet unrecognized provider credential
                // before the full-content scanner sees it. Omit the oversized
                // field instead of exposing unscannable head/tail fragments.
                return ("[truncated]".into(), true);
            }
        }
        (output, total > 1024 * 1024)
    }
    pub fn redact_value(&self, value: &mut Value) -> crate::Result<()> {
        if self.fingerprints.is_empty() {
            return Ok(());
        }
        fn visit(
            mask: &Mask,
            value: &mut Value,
            path: &str,
            cut: &mut Vec<String>,
        ) -> crate::Result<()> {
            match value {
                Value::String(text) => {
                    let (safe, truncated) = mask.redact_text(text);
                    *text = safe;
                    if truncated {
                        cut.push(path.into());
                    }
                }
                Value::Array(items) => {
                    for (index, item) in items.iter_mut().enumerate() {
                        visit(mask, item, &format!("{path}/{index}"), cut)?;
                    }
                }
                Value::Object(items) => {
                    let original = std::mem::take(items);
                    for (name, mut item) in original {
                        let safe_name = mask.redact(&name);
                        visit(
                            mask,
                            &mut item,
                            &format!("{path}/{}", safe_name.replace('~', "~0").replace('/', "~1")),
                            cut,
                        )?;
                        if items.insert(safe_name, item).is_some() {
                            return Err("Secret masking produced conflicting field names.".into());
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        }
        let mut cut = Vec::new();
        visit(self, value, "", &mut cut)?;
        if !cut.is_empty() {
            let marked = value["_intercatTruncated"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            cut.extend(marked);
            value["_intercatTruncated"] = serde_json::json!(cut);
        }
        Ok(())
    }
    pub fn new(key: &[u8; 32], mut fingerprints: Vec<Fingerprint>) -> crate::Result<Self> {
        if fingerprints.len() > 256
            || fingerprints.iter().any(|f| {
                f.byte_len == 0
                    || f.byte_len > MAX_TAIL
                    || f.name.is_empty()
                    || f.name.len() > 128
                    || !f
                        .name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            })
        {
            return Err("Invalid output fingerprints.".into());
        }
        // Stable precedence when two names refer to the same value.
        fingerprints.sort_by(|a, b| a.name.cmp(&b.name));
        let max_len = fingerprints.iter().map(|f| f.byte_len).max().unwrap_or(0);
        Ok(Self {
            key: Some(hmac::Key::new(hmac::HMAC_SHA256, key)),
            fingerprints,
            max_len,
        })
    }

    fn release(&self, raw: &str, final_text: bool, covered: &[Covered], masking: bool) -> Released {
        let cut = if final_text {
            raw.len()
        } else {
            raw.len().saturating_sub(self.max_len.saturating_sub(1))
        };
        self.release_to(raw, cut, covered, masking)
    }
    fn release_to(
        &self,
        raw: &str,
        mut cut: usize,
        covered: &[Covered],
        masking: bool,
    ) -> Released {
        while !raw.is_char_boundary(cut) {
            cut -= 1;
        }
        let mut ranges = covered.to_vec();
        if let Some(key) = &self.key {
            let mut lengths = BTreeMap::<usize, Vec<&Fingerprint>>::new();
            for fingerprint in &self.fingerprints {
                lengths
                    .entry(fingerprint.byte_len)
                    .or_default()
                    .push(fingerprint);
            }
            for (start, _) in raw.char_indices() {
                for (len, fingerprints) in &lengths {
                    let end = start + len;
                    if end > raw.len() || !raw.is_char_boundary(end) {
                        continue;
                    }
                    let tag = hmac::sign(key, &raw.as_bytes()[start..end]);
                    if let Some(found) = fingerprints.iter().find(|f| {
                        use subtle::ConstantTimeEq;
                        bool::from(tag.as_ref().ct_eq(&f.tag))
                    }) {
                        ranges.push((start, end, found.name.clone()));
                    }
                }
            }
        }
        ranges.sort_by_key(|(start, end, _)| (*start, *end));
        let mut merged: Vec<Covered> = Vec::new();
        for (start, end, name) in ranges {
            if let Some(last) = merged.last_mut()
                && start < last.1
            {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end, name));
            }
        }
        // Retain the raw suffix AND the spans already proven secret. A later
        // match can overlap a preceding one without exposing its remaining bytes.
        let mut output = String::new();
        let mut position = 0;
        for (start, end, name) in &merged {
            if *start >= cut {
                break;
            }
            output.push_str(&raw[position..*start]);
            if *start != 0 || !masking {
                if name.is_empty() {
                    output.push_str("[redacted]");
                } else {
                    output.push_str(&format!("<secret:{name}>"));
                }
            }
            position = (*end).min(cut);
        }
        output.push_str(&raw[position..cut]);
        let still_masking = if cut == 0 {
            masking
        } else {
            merged
                .iter()
                .any(|(start, end, _)| *start < cut && *end >= cut)
        };
        Released {
            text: output,
            used: cut,
            masking: still_masking,
            covered: merged
                .into_iter()
                .filter(|(_, end, _)| *end > cut)
                .map(|(start, end, name)| (start.saturating_sub(cut), end - cut, name))
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preview {
    pub session_id: String,
    pub item_id: String,
    pub turn_id: String,
    pub text: String,
    pub truncated: bool,
}
struct Item {
    preview: Preview,
    tail: String,
    covered: Vec<Covered>,
    masking: bool,
    revision: u64,
    completed: bool,
    awaiting_scan: bool,
    raw: Option<Raw>,
}

struct Raw {
    generation: u64,
    version: u64,
    attempted: u64,
    applied: u64,
    text: String,
    truncated: bool,
}

pub(crate) struct Pending {
    id: String,
    generation: u64,
    version: u64,
    raw: String,
    truncated: bool,
    mask: Mask,
}
impl Pending {
    pub fn scan(&self) -> Result<String, &'static str> {
        let scanned = crate::secret_prefix::scan(&self.raw)?;
        let cut = scanned.cut.min(
            self.raw
                .len()
                .saturating_sub(self.mask.max_len.saturating_sub(1)),
        );
        Ok(self
            .mask
            .release_to(&self.raw, cut, &scanned.ranges, false)
            .text)
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Privacy {
    #[default]
    KnownValues,
    Rules,
}

#[derive(Default)]
pub struct Progress {
    session: Option<String>,
    active_turn: Option<String>,
    mask: Mask,
    privacy: Privacy,
    generation: u64,
    items: BTreeMap<String, Item>,
    early: BTreeMap<String, (String, bool)>,
    buffer_early: bool,
    latest: Option<String>,
}
impl Progress {
    pub fn replace_mask(&mut self, mask: Mask) {
        self.clear();
        self.mask = mask;
    }
    /// Call before dispatching work. A new connection never inherits raw tails.
    pub fn configure(
        &mut self,
        session: String,
        mask: Mask,
        privacy: Privacy,
        active_turn: Option<String>,
    ) {
        self.clear();
        self.session = Some(session);
        self.active_turn = active_turn;
        self.mask = mask;
        self.privacy = privacy;
    }
    pub fn clear(&mut self) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("Progress generation exhausted");
        self.items.clear();
        self.early.clear();
        self.buffer_early = false;
        self.latest = None;
    }
    /// Only a submitted command may open a new display turn. Notifications
    /// alone cannot revive a completed or unrelated turn.
    pub fn expect_turn(&mut self, session: &str, turn: &str) {
        if self.session.as_deref() != Some(session) || turn.is_empty() || turn.len() > 128 {
            return;
        }
        if self.active_turn.as_deref() != Some(turn) {
            self.clear();
            self.active_turn = Some(turn.into());
            self.buffer_early = true;
        }
    }
    pub fn snapshot(&self) -> Option<Preview> {
        let item = self.items.get(self.latest.as_ref()?)?;
        (!item.preview.text.is_empty()).then(|| item.preview.clone())
    }
    /// The reader only buffers repair text. A separate worker scans it before
    /// apply() can expose it, without delaying replies or priority control.
    pub fn receive(&mut self, frame: &Value) {
        self.observe(frame, true, false);
    }
    pub fn scanned(&mut self, frame: &Value) {
        if self.privacy == Privacy::Rules
            && matches!(
                frame["method"].as_str(),
                Some("item/started" | "item/updated" | "item/completed")
            )
            && (frame["method"] == "item/completed"
                || frame["params"]["item"]["status"] != "inProgress")
        {
            self.observe(frame, false, true);
        }
    }
    pub(crate) fn pending(&mut self) -> Option<Pending> {
        if self.privacy != Privacy::Rules {
            return None;
        }
        let id = self.latest.as_ref()?;
        let item = self.items.get_mut(id)?;
        // A fast burst can finish before the worker is scheduled. Its buffered
        // deltas still need scanning; completion never adds raw full-item text.
        let raw = item.raw.as_mut()?;
        if raw.version == raw.attempted {
            return None;
        }
        raw.attempted = raw.version;
        Some(Pending {
            id: id.clone(),
            generation: raw.generation,
            version: raw.version,
            raw: raw.text.clone(),
            truncated: raw.truncated,
            mask: self.mask.clone(),
        })
    }
    pub(crate) fn apply(&mut self, pending: Pending, mut safe: String) {
        let Some(item) = self.items.get_mut(&pending.id) else {
            return;
        };
        let Some(raw) = item.raw.as_mut() else {
            return;
        };
        if raw.generation != pending.generation
            || raw.applied > pending.version
            || !raw.text.starts_with(&pending.raw)
        {
            return;
        }
        raw.applied = pending.version;
        let overflow = safe.len() > MAX_TEXT;
        if overflow {
            let mut end = MAX_TEXT;
            while !safe.is_char_boundary(end) {
                end -= 1;
            }
            safe.truncate(end);
        }
        item.preview.text = safe;
        item.preview.truncated = pending.truncated || overflow;
    }
    fn observe(&mut self, frame: &Value, deltas: bool, verified: bool) {
        let params = &frame["params"];
        if params["sessionId"].as_str() != self.session.as_deref() {
            return;
        }
        let method = frame["method"].as_str().unwrap_or("");
        if method == "turn/started" {
            // The request (or recovered active turn) already selected it.
            // Replayed starts must not discard its accumulated text.
            return;
        }
        if method == "turn/completed" {
            if params["turnId"].as_str() == self.active_turn.as_deref() {
                self.clear();
                self.active_turn = None;
            }
            return;
        }
        if matches!(method, "view/gap" | "session/viewHealthChanged") {
            self.clear();
            return;
        }
        if matches!(method, "item/started" | "item/updated" | "item/completed") {
            let value = &params["item"];
            let Some(id) = value["itemId"].as_str() else {
                return;
            };
            let Some(turn) = value["turnId"].as_str() else {
                return;
            };
            if value["kind"] != "agentMessage"
                || id.len() > 128
                || Some(turn) != self.active_turn.as_deref()
            {
                return;
            }
            if !self.items.contains_key(id)
                && !self.early.contains_key(id)
                && self.items.len() + self.early.len() == MAX_ITEMS
            {
                // Never forget and then accept a suffix of an overflowed item.
                return;
            }
            let revision = value["revision"].as_u64().unwrap_or(0);
            let terminal = method == "item/completed" || value["status"] != "inProgress";
            if self.items.get(id).is_some_and(|item| {
                item.revision > revision
                    || (item.revision == revision
                        && !(verified && item.completed && item.awaiting_scan))
            }) {
                return;
            }
            if self.privacy == Privacy::Rules && !verified && terminal {
                // Keep the last verified delta prefix until the completed
                // content scanner returns. Do not enqueue the full item as
                // if it were a newly arrived streaming fragment.
                if let Some(item) = self.items.get_mut(id) {
                    item.revision = revision;
                    item.completed = true;
                    item.awaiting_scan = true;
                }
                return;
            }
            self.generation = self
                .generation
                .checked_add(1)
                .expect("Progress generation exhausted");
            let item = Item {
                preview: Preview {
                    session_id: self.session.clone().unwrap(),
                    item_id: id.into(),
                    turn_id: turn.into(),
                    text: String::new(),
                    truncated: false,
                },
                tail: String::new(),
                covered: vec![],
                masking: false,
                revision,
                completed: terminal,
                awaiting_scan: false,
                raw: (self.privacy == Privacy::Rules && !verified).then(|| Raw {
                    generation: self.generation,
                    version: 0,
                    attempted: 0,
                    applied: 0,
                    text: String::new(),
                    truncated: false,
                }),
            };
            self.items.insert(id.into(), item);
            let early = self.early.remove(id);
            if let Some(text) = value["text"].as_str() {
                self.append(id, text, terminal);
            }
            if value["text"].as_str().is_none_or(str::is_empty)
                && !terminal
                && let Some((text, truncated)) = early
            {
                self.append(id, &text, false);
                if truncated {
                    let item = self.items.get_mut(id).unwrap();
                    if let Some(raw) = &mut item.raw {
                        raw.truncated = true;
                    } else {
                        item.preview.truncated = true;
                    }
                }
            }
            self.latest = Some(id.into());
        } else if deltas
            && method == "item/delta"
            && (params["field"].is_null() || params["field"] == "text")
            && let (Some(id), Some(text)) = (params["itemId"].as_str(), params["delta"].as_str())
        {
            if self.items.get(id).is_some_and(|item| !item.completed) {
                self.append(id, text, false);
                self.latest = Some(id.into());
            } else if !self.items.contains_key(id)
                && self.buffer_early
                && self.active_turn.is_some()
                && id.len() <= 128
                && (self.early.contains_key(id) || self.items.len() + self.early.len() < MAX_ITEMS)
            {
                let (buffer, truncated) = self.early.entry(id.into()).or_default();
                if !*truncated {
                    let mut end = text.len().min(MAX_TEXT - buffer.len());
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    buffer.push_str(&text[..end]);
                    *truncated = end < text.len();
                }
            }
        }
    }
    fn append(&mut self, id: &str, text: &str, final_text: bool) {
        let item = self.items.get_mut(id).unwrap();
        if let Some(raw) = &mut item.raw {
            if raw.truncated {
                return;
            }
            let mut end = text.len().min(MAX_TEXT - raw.text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            raw.text.push_str(&text[..end]);
            raw.truncated = end < text.len();
            raw.version += 1;
            return;
        }
        if item.preview.truncated {
            return;
        }
        // Bound temporary scanning memory too, not just the displayed prefix.
        let mut remaining = text;
        while !remaining.is_empty() {
            let mut end = remaining.len().min(MAX_TEXT);
            while !remaining.is_char_boundary(end) {
                end -= 1;
            }
            item.tail.push_str(&remaining[..end]);
            remaining = &remaining[end..];
            let released = self.mask.release(
                &item.tail,
                final_text && remaining.is_empty(),
                &item.covered,
                item.masking,
            );
            item.tail.drain(..released.used);
            item.covered = released.covered;
            item.masking = released.masking;
            let safe = released.text;
            let available = MAX_TEXT - item.preview.text.len();
            if safe.len() > available {
                let mut end = available;
                while !safe.is_char_boundary(end) {
                    end -= 1;
                }
                item.preview.text.push_str(&safe[..end]);
                item.preview.truncated = true;
                item.tail.clear();
                item.covered.clear();
                break;
            }
            item.preview.text.push_str(&safe);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mask(values: &[(&str, &str)]) -> Mask {
        let key = [7; 32];
        let hmac = hmac::Key::new(hmac::HMAC_SHA256, &key);
        Mask::new(
            &key,
            values
                .iter()
                .map(|(name, value)| Fingerprint {
                    name: (*name).into(),
                    byte_len: value.len(),
                    tag: hmac::sign(&hmac, value.as_bytes())
                        .as_ref()
                        .try_into()
                        .unwrap(),
                })
                .collect(),
        )
        .unwrap()
    }
    fn item(method: &str, id: &str, revision: u64, text: &str) -> Value {
        json!({"method":method,"params":{"sessionId":"s","item":{"itemId":id,"turnId":"t","kind":"agentMessage","revision":revision,"text":text,"status":if method=="item/completed"{"completed"}else{"inProgress"}}}})
    }
    fn delta(id: &str, text: &str) -> Value {
        json!({"method":"item/delta","params":{"sessionId":"s","itemId":id,"delta":text}})
    }

    #[test]
    #[ignore = "offline SDK oracle; set INTERCAT_PROGRESS_ORACLE to the prepared case directory"]
    fn sdk_progress_comparison() -> crate::Result<()> {
        let directory = std::path::PathBuf::from(std::env::var("INTERCAT_PROGRESS_ORACLE")?);
        let fixture: Value =
            serde_json::from_slice(&std::fs::read(directory.join("fixture.json"))?)?;
        let oracle: Value = serde_json::from_slice(&std::fs::read(directory.join("ts.json"))?)?;
        let mut results = Vec::new();
        for (case, expected) in fixture["cases"]
            .as_array()
            .unwrap()
            .iter()
            .zip(oracle["cases"].as_array().unwrap())
        {
            assert_eq!(case["id"], expected["id"]);
            for privacy in [Privacy::KnownValues, Privacy::Rules] {
                let mut stream = Progress::default();
                stream.configure(
                    case["sessionId"].as_str().unwrap().into(),
                    Mask::default(),
                    privacy,
                    case["initialTurn"].as_str().map(str::to_owned),
                );
                let mut steps = Vec::new();
                let mut scans = Vec::new();
                let mut previous_sdk = Value::Null;
                for (index, action) in case["actions"].as_array().unwrap().iter().enumerate() {
                    if let Some(params) = action.get("send") {
                        stream.expect_turn(
                            params["sessionId"].as_str().unwrap(),
                            params["commandId"].as_str().unwrap(),
                        );
                    } else {
                        let frame = &action["frame"];
                        stream.receive(frame);
                        if privacy == Privacy::Rules {
                            stream.scanned(&crate::recording::sanitize(frame.clone(), &[]));
                        }
                    }
                    let mut wanted = expected["steps"][index].clone();
                    let mut scanned_text = None;
                    let mut attempted = false;
                    if let Some(pending) = stream.pending() {
                        attempted = true;
                        assert_eq!(
                            pending.raw,
                            wanted["text"].as_str().unwrap_or(""),
                            "SDK-ordered scan input: {} / {index}",
                            case["id"]
                        );
                        let previous = serde_json::to_value(stream.snapshot())?;
                        match pending.scan() {
                            Ok(safe) => {
                                scans.push(json!({"step":index,"result":"verified"}));
                                scanned_text = Some(safe.clone());
                                stream.apply(pending, safe);
                            }
                            Err(error) => {
                                scans
                                    .push(json!({"step":index,"result":"withheld","reason":error}));
                                assert_eq!(
                                    serde_json::to_value(stream.snapshot())?,
                                    previous,
                                    "Failed scans must not release new text"
                                );
                            }
                        }
                    }
                    let actual = serde_json::to_value(stream.snapshot())?;
                    if privacy == Privacy::Rules && expected["finalized"][index] != true {
                        if !wanted.is_null() && wanted != previous_sdk {
                            assert!(
                                attempted,
                                "Changed SDK text must enter the scanner: {} / {index}",
                                case["id"]
                            );
                        }
                        if let Some(safe) = scanned_text {
                            if safe.is_empty() {
                                wanted = Value::Null;
                            } else {
                                wanted["text"] = safe.into();
                            }
                        } else if !actual.is_null() {
                            // A scanner may withhold a suffix or refuse an expensive
                            // input. Only a previously verified SDK prefix may remain.
                            assert!(
                                wanted["text"]
                                    .as_str()
                                    .unwrap_or("")
                                    .starts_with(actual["text"].as_str().unwrap())
                            );
                            wanted["text"] = actual["text"].clone();
                        } else {
                            wanted = Value::Null;
                        }
                    }
                    assert_eq!(
                        actual,
                        wanted,
                        "case {} / {} / step {index}",
                        case["id"],
                        if privacy == Privacy::Rules {
                            "rules"
                        } else {
                            "known-values"
                        }
                    );
                    previous_sdk = expected["steps"][index].clone();
                    steps.push(actual);
                }
                results.push(json!({"id":case["id"],"privacy":if privacy == Privacy::Rules {"rules"} else {"known-values"},"steps":steps,"scans":scans}));
            }
        }
        std::fs::write(
            directory.join("engine.json"),
            serde_json::to_vec(&json!({"cases":results}))?,
        )?;
        Ok(())
    }

    fn duplicate_revisions(privacy: Privacy) {
        let mut stream = Progress::default();
        stream.configure("s".into(), Mask::default(), privacy, Some("t".into()));
        stream.receive(&item("item/started", "i", 1, ""));
        stream.receive(&delta("i", "already displayed\n"));
        if let Some(pending) = stream.pending() {
            let safe = pending.scan().unwrap();
            stream.apply(pending, safe);
        }
        let displayed = stream.snapshot().unwrap().text;
        assert!(!displayed.is_empty());
        for method in ["item/started", "item/updated"] {
            stream.receive(&item(method, "i", 1, "stale duplicate\n"));
            assert_eq!(stream.snapshot().unwrap().text, displayed, "{method}");
        }
    }

    #[test]
    fn duplicate_revisions_cannot_replace_live_text() {
        duplicate_revisions(Privacy::KnownValues);
    }

    #[test]
    fn duplicate_revisions_cannot_replace_scanned_live_text() {
        duplicate_revisions(Privacy::Rules);
    }

    #[test]
    fn terminal_item_updates_never_accept_late_deltas() {
        for status in ["completed", "failed", "futureTerminal"] {
            let mut stream = Progress::default();
            stream.configure(
                "s".into(),
                Mask::default(),
                Privacy::KnownValues,
                Some("t".into()),
            );
            stream.receive(&item("item/started", "i", 1, ""));
            stream.receive(&item("item/completed", "i", 2, "first final"));
            let mut update = item("item/updated", "i", 3, "corrected final");
            update["params"]["item"]["status"] = status.into();
            stream.receive(&update);
            stream.receive(&delta("i", " late fragment"));
            assert_eq!(stream.snapshot().unwrap().text, "corrected final");
        }
    }

    #[test]
    fn terminal_repair_updates_wait_for_a_verified_body() {
        for status in ["completed", "failed", "futureTerminal"] {
            let mut stream = Progress::default();
            stream.configure(
                "s".into(),
                Mask::default(),
                Privacy::Rules,
                Some("t".into()),
            );
            stream.receive(&item("item/started", "i", 1, "safe prefix\n"));
            let pending = stream.pending().unwrap();
            let safe = pending.scan().unwrap();
            stream.apply(pending, safe.clone());
            let mut update = item("item/updated", "i", 2, "unscanned terminal body");
            update["params"]["item"]["status"] = status.into();
            stream.receive(&update);
            stream.receive(&delta("i", " must not append"));
            assert_eq!(stream.snapshot().unwrap().text, safe);
            update["params"]["item"]["text"] = "verified terminal body".into();
            stream.scanned(&update);
            stream.receive(&delta("i", " late fragment"));
            assert_eq!(stream.snapshot().unwrap().text, "verified terminal body");
        }
    }

    #[test]
    fn early_deltas_wait_for_the_item_without_losing_the_prefix() {
        for privacy in [Privacy::KnownValues, Privacy::Rules] {
            let mut stream = Progress::default();
            stream.configure("s".into(), Mask::default(), privacy, None);
            stream.expect_turn("s", "t");
            stream
                .receive(&json!({"method":"turn/started","params":{"sessionId":"s","turnId":"t"}}));
            stream.receive(&delta("i", "日本語の先頭\n"));
            assert!(stream.snapshot().is_none());
            stream.receive(&item("item/started", "i", 1, ""));
            stream.receive(&delta("i", "後半\n"));
            if let Some(pending) = stream.pending() {
                let safe = pending.scan().unwrap();
                stream.apply(pending, safe);
            }
            let text = stream.snapshot().unwrap().text;
            assert_eq!(text.trim_end(), "日本語の先頭\n後半");
        }
    }

    #[test]
    fn early_secret_fragments_are_masked_across_the_item_start() {
        for privacy in [Privacy::KnownValues, Privacy::Rules] {
            let mut stream = Progress::default();
            stream.configure("s".into(), mask(&[("TOKEN", "猫token🔑値")]), privacy, None);
            stream.expect_turn("s", "t");
            stream.receive(&delta("i", "safe 猫token"));
            assert!(stream.snapshot().is_none());
            stream.receive(&item("item/started", "i", 1, ""));
            stream.receive(&delta(
                "i",
                "🔑値 safely finished with enough trailing text.\n",
            ));
            if let Some(pending) = stream.pending() {
                let safe = pending.scan().unwrap();
                stream.apply(pending, safe);
            }
            let text = stream.snapshot().unwrap().text;
            assert!(
                text.starts_with("safe <secret:TOKEN> safely finished"),
                "{text}"
            );
            assert!(!text.contains("猫token"));
        }
    }

    #[test]
    fn early_buffers_share_the_item_limit_and_reset_at_gaps_and_new_commands() {
        let mut stream = Progress::default();
        stream.configure("s".into(), Mask::default(), Privacy::KnownValues, None);
        stream.expect_turn("s", "t");
        for index in 0..MAX_ITEMS + 1 {
            stream.receive(&delta(&format!("i{index}"), &"猫".repeat(MAX_TEXT)));
        }
        assert_eq!(stream.early.len(), MAX_ITEMS);
        assert!(
            stream
                .early
                .values()
                .all(|(text, truncated)| text.len() <= MAX_TEXT && *truncated)
        );
        stream.receive(&item("item/started", "i0", 1, ""));
        assert_eq!(stream.items.len() + stream.early.len(), MAX_ITEMS);
        let first = stream.snapshot().unwrap();
        assert!(first.truncated);
        stream.receive(&delta("i0", "must not restart from a suffix"));
        assert_eq!(stream.snapshot().unwrap().text, first.text);
        stream.receive(&item("item/started", "i16", 1, ""));
        assert!(!stream.items.contains_key("i16"));
        stream.receive(&json!({"method":"view/gap","params":{"sessionId":"s"}}));
        stream.receive(&delta("gap", "unknown missing prefix"));
        assert!(stream.early.is_empty());
        stream.expect_turn("s", "next");
        stream.receive(&delta("fresh", "new prefix\n"));
        let mut fresh = item("item/started", "fresh", 1, "");
        fresh["params"]["item"]["turnId"] = "next".into();
        stream.receive(&fresh);
        assert_eq!(stream.snapshot().unwrap().text, "new prefix\n");
    }

    #[test]
    fn only_submitted_or_recovered_turns_can_supply_a_preview() {
        let mut stream = Progress::default();
        stream.configure("s".into(), Mask::default(), Privacy::KnownValues, None);
        let started = json!({"method":"turn/started","params":{"sessionId":"s","turnId":"t"}});
        stream.receive(&started);
        stream.receive(&item("item/started", "i", 1, "unsolicited"));
        assert!(stream.snapshot().is_none());
        stream.expect_turn("s", "t");
        stream.receive(&item("item/started", "i", 1, ""));
        stream.receive(&delta("i", "current\n"));
        stream.receive(&started);
        assert_eq!(stream.snapshot().unwrap().text, "current\n");
        stream.receive(&json!({"method":"turn/completed","params":{"sessionId":"s","turnId":"t"}}));
        stream.receive(&started);
        stream.receive(&item("item/updated", "i", 2, "replayed"));
        assert!(stream.snapshot().is_none());
        stream.expect_turn("other", "t");
        stream.receive(&item("item/updated", "i", 3, "wrong session command"));
        assert!(stream.snapshot().is_none());
    }

    #[test]
    fn every_unicode_split_keeps_the_secret_out_of_every_preview() {
        let secret = "猫token🔑値";
        let whole = format!("before {secret} after plenty of safe text");
        for split in whole
            .char_indices()
            .map(|(index, _)| index)
            .chain([whole.len()])
        {
            let mut stream = Progress::default();
            stream.configure(
                "s".into(),
                mask(&[("TOKEN", secret)]),
                Privacy::KnownValues,
                Some("t".into()),
            );
            stream.receive(&item("item/started", "i", 0, ""));
            stream.receive(&delta("i", &whole[..split]));
            let first = stream.snapshot().map(|v| v.text).unwrap_or_default();
            assert!(!first.contains(secret), "split {split}");
            // None of the secret's bytes can appear between the two safe sides.
            assert!(
                first == "before "
                    || "before ".starts_with(&first)
                    || first.starts_with("before <secret:TOKEN>"),
                "{split}: {first}"
            );
            stream.receive(&delta("i", &whole[split..]));
            let current = stream.snapshot().unwrap();
            assert!(!current.text.contains(secret));
            assert!(current.text.starts_with("before <secret:TOKEN>"));
            stream.receive(&item("item/completed", "i", 1, &whole));
            assert_eq!(
                stream.snapshot().unwrap().text,
                "before <secret:TOKEN> after plenty of safe text"
            );
        }
    }

    #[test]
    fn overlapping_values_and_different_lengths_mask_the_entire_union() {
        let matcher = mask(&[("FIRST", "abc"), ("SECOND", "bcdef"), ("WHOLE", "abcdef")]);
        let Released {
            text, used: cut, ..
        } = matcher.release("safe abcdef end", true, &[], false);
        assert_eq!(text, "safe <secret:FIRST> end");
        assert_eq!(cut, 15);
        let mut stream = Progress::default();
        stream.configure("s".into(), matcher, Privacy::KnownValues, Some("t".into()));
        stream.receive(&item("item/started", "i", 0, ""));
        for letter in "safe abcdef end more".chars() {
            stream.receive(&delta("i", &letter.to_string()));
            let text = stream.snapshot().map(|v| v.text).unwrap_or_default();
            assert!(!text.contains("abc"));
            assert!(!text.contains("bcdef"));
        }
    }

    #[test]
    fn a_later_overlapping_match_cannot_reveal_its_suffix() {
        let mut stream = Progress::default();
        stream.configure(
            "s".into(),
            mask(&[("FIRST", "abc"), ("SECOND", "cdefgh")]),
            Privacy::KnownValues,
            Some("t".into()),
        );
        stream.receive(&item("item/started", "i", 0, ""));
        for letter in "abcdefgh0123456789".chars() {
            stream.receive(&delta("i", &letter.to_string()));
            let text = stream.snapshot().map(|v| v.text).unwrap_or_default();
            let visible = text
                .replace("<secret:FIRST>", "")
                .replace("<secret:SECOND>", "");
            assert!(
                !visible.contains(['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h']),
                "{text}"
            );
            assert!(stream.items["i"].tail.len() < 6);
        }
        assert_eq!(stream.snapshot().unwrap().text, "<secret:FIRST>01234");
    }

    #[test]
    fn gaps_resets_sessions_fields_and_revisions_cannot_stitch_unrelated_text() {
        let mut stream = Progress::default();
        stream.configure(
            "s".into(),
            Mask::default(),
            Privacy::KnownValues,
            Some("t".into()),
        );
        stream.receive(&item("item/started", "i", 1, "hello"));
        let mut other = delta("i", "foreign");
        other["params"]["sessionId"] = json!("other");
        stream.receive(&other);
        let mut reasoning = delta("i", "hidden reasoning");
        reasoning["params"]["field"] = json!("summary.0");
        stream.receive(&reasoning);
        reasoning["params"]["field"] = json!(3);
        stream.receive(&reasoning);
        assert_eq!(stream.snapshot().unwrap().text, "hello");
        stream.receive(&item("item/completed", "i", 2, "complete"));
        stream.receive(&delta("i", "late"));
        stream.receive(&item("item/updated", "i", 1, "stale"));
        assert_eq!(stream.snapshot().unwrap().text, "complete");
        stream.receive(&item("item/updated", "i", 3, "revised"));
        assert_eq!(stream.snapshot().unwrap().text, "revised");
        stream.receive(
            &json!({"method":"turn/completed","params":{"sessionId":"s","turnId":"older"}}),
        );
        assert_eq!(stream.snapshot().unwrap().text, "revised");
        stream.receive(&json!({"method":"turn/completed","params":{"sessionId":"s","turnId":"t"}}));
        stream.receive(&item("item/updated", "i", 4, "late old turn"));
        assert!(stream.snapshot().is_none());
        stream
            .receive(&json!({"method":"turn/started","params":{"sessionId":"s","turnId":"next"}}));
        stream.expect_turn("s", "next");
        stream.receive(&item("item/updated", "i", 5, "another old turn"));
        assert!(stream.snapshot().is_none());
        let mut fresh = item("item/started", "fresh", 1, "fresh turn");
        fresh["params"]["item"]["turnId"] = json!("next");
        stream.receive(&fresh);
        assert_eq!(stream.snapshot().unwrap().text, "fresh turn");
        stream.receive(&json!({"method":"view/gap","params":{"sessionId":"s"}}));
        stream.receive(&delta("i", "suffix after missing text"));
        assert!(stream.snapshot().is_none());
        stream.configure(
            "new".into(),
            Mask::default(),
            Privacy::KnownValues,
            Some("t".into()),
        );
        stream.receive(&item("item/started", "i", 4, "old session"));
        assert!(stream.snapshot().is_none());
    }

    #[test]
    fn retention_is_bounded_and_overflowed_items_do_not_restart_at_a_suffix() {
        let mut stream = Progress::default();
        stream.configure(
            "s".into(),
            Mask::default(),
            Privacy::KnownValues,
            Some("t".into()),
        );
        stream.receive(&item("item/started", "i", 0, ""));
        stream.receive(&delta("i", &"猫".repeat(MAX_TEXT)));
        assert!(stream.snapshot().unwrap().text.len() <= MAX_TEXT);
        assert!(stream.snapshot().unwrap().truncated);
        assert!(stream.items["i"].tail.is_empty());
        stream.receive(&delta("i", "not appended"));
        assert!(!stream.snapshot().unwrap().text.contains("appended"));
        for index in 0..32 {
            stream.receive(&item("item/started", &format!("i{index}"), 0, "hello"));
        }
        assert_eq!(stream.items.len(), MAX_ITEMS);
        stream.receive(&delta("i31", "suffix"));
        assert!(!stream.snapshot().unwrap().text.contains("suffix"));
    }

    #[test]
    fn repair_does_not_expose_unscanned_text() {
        let mut stream = Progress::default();
        stream.configure(
            "s".into(),
            Mask::default(),
            Privacy::Rules,
            Some("t".into()),
        );
        stream.receive(&item("item/started", "i", 0, "raw token"));
        stream.receive(&delta("i", "raw secret"));
        stream.scanned(&item("item/started", "i", 0, "partial unrecognized token"));
        assert!(stream.snapshot().is_none());
        stream.scanned(&item("item/completed", "i", 1, "[redacted]"));
        assert_eq!(stream.snapshot().unwrap().text, "[redacted]");
    }

    #[test]
    fn repair_completion_waits_for_full_scan_and_retains_verified_delta() {
        let mut stream = Progress::default();
        stream.configure(
            "s".into(),
            Mask::default(),
            Privacy::Rules,
            Some("t".into()),
        );
        stream.receive(&item("item/started", "i", 0, ""));
        stream.receive(&delta("i", "途中の検証済み表示。"));
        let pending = stream.pending().unwrap();
        let safe = pending.scan().unwrap();
        stream.apply(pending, safe.clone());
        stream.receive(&item(
            "item/completed",
            "i",
            1,
            "unscanned completed secret",
        ));
        assert!(stream.pending().is_none());
        assert_eq!(stream.snapshot().unwrap().text, safe);
        stream.scanned(&item("item/completed", "i", 1, "[redacted]"));
        assert_eq!(stream.snapshot().unwrap().text, "[redacted]");
    }

    #[test]
    fn completion_does_not_discard_unscanned_deltas_from_a_fast_burst() {
        let mut stream = Progress::default();
        stream.configure(
            "s".into(),
            Mask::default(),
            Privacy::Rules,
            Some("t".into()),
        );
        stream.receive(&item("item/started", "i", 1, ""));
        stream.receive(&delta("i", "受信した途中の本文。"));
        stream.receive(&item(
            "item/completed",
            "i",
            2,
            "raw completed text must not be used",
        ));
        let pending = stream.pending().unwrap();
        assert_eq!(pending.raw, "受信した途中の本文。");
        let safe = pending.scan().unwrap();
        stream.apply(pending, safe);
        assert!(stream.snapshot().unwrap().text.starts_with("受信した"));
    }

    #[test]
    fn repair_merges_known_values_with_provider_matches_before_releasing() {
        let token = format!("ghp_{}", "A6k9P2mR".repeat(4) + "X4v7");
        let known = format!("{} end", &token[token.len() - 8..]);
        let mut stream = Progress::default();
        stream.configure(
            "s".into(),
            mask(&[("TOKEN", &known)]),
            Privacy::Rules,
            Some("t".into()),
        );
        stream.receive(&item("item/started", "i", 0, ""));
        let text = format!("安全です。\n{token} end\n確認しています。\n");
        for ch in text.chars() {
            stream.receive(&delta("i", &ch.to_string()));
            let pending = stream.pending().unwrap();
            let safe = pending.scan().unwrap();
            assert!(!safe.contains("A6k9"));
            assert!(!safe.contains(" end"));
            stream.apply(pending, safe);
        }
        assert!(stream.snapshot().unwrap().text.starts_with("安全です。\n"));
        assert!(stream.snapshot().unwrap().text.contains("[redacted]"));
    }

    #[test]
    fn pending_scans_cannot_overwrite_replacements_completion_or_gaps() {
        let mut stream = Progress::default();
        stream.configure(
            "s".into(),
            Mask::default(),
            Privacy::Rules,
            Some("t".into()),
        );
        stream.receive(&item("item/started", "i", 0, "first"));
        let old = stream.pending().unwrap();
        stream.receive(&item("item/updated", "i", 1, "replacement"));
        stream.apply(old, "obsolete".into());
        assert!(stream.snapshot().is_none());
        let old = stream.pending().unwrap();
        stream.receive(&item("item/completed", "i", 2, "unscanned"));
        stream.scanned(&item("item/completed", "i", 2, "verified"));
        stream.apply(old, "obsolete".into());
        assert_eq!(stream.snapshot().unwrap().text, "verified");
        stream.receive(&item("item/updated", "i", 3, "after completion"));
        let old = stream.pending().unwrap();
        stream.receive(&json!({"method":"view/gap","params":{"sessionId":"s"}}));
        stream.receive(&item("item/started", "i", 0, "fresh"));
        stream.apply(old, "obsolete".into());
        assert!(stream.snapshot().is_none());
        let pending = stream.pending().unwrap();
        stream.receive(&delta("i", " appended"));
        stream.apply(pending, "verified prefix".into());
        assert_eq!(stream.snapshot().unwrap().text, "verified prefix");
        let pending = stream.pending().unwrap();
        stream.configure(
            "s".into(),
            Mask::default(),
            Privacy::Rules,
            Some("t".into()),
        );
        stream.receive(&item("item/started", "i", 0, "new configuration"));
        stream.apply(pending, "obsolete".into());
        assert!(stream.snapshot().is_none());
    }

    #[test]
    fn repair_input_is_bounded_and_overflow_never_restarts_from_a_suffix() {
        let mut stream = Progress::default();
        stream.configure(
            "s".into(),
            Mask::default(),
            Privacy::Rules,
            Some("t".into()),
        );
        stream.receive(&item("item/started", "i", 0, ""));
        stream.receive(&delta("i", &"猫".repeat(MAX_TEXT)));
        let pending = stream.pending().unwrap();
        assert!(pending.raw.len() <= MAX_TEXT);
        assert!(pending.truncated);
        stream.apply(pending, "verified prefix".into());
        stream.receive(&delta("i", "must not append"));
        assert!(stream.pending().is_none());
        assert!(stream.snapshot().unwrap().truncated);
    }
}
