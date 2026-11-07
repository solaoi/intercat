//! The Editor's finite choices. Model scores select an operation; they never
//! become executable JSON, a shell command, or a path supplied by the model.
use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use unicode_normalization::UnicodeNormalization;

const PREFIX: &str = "Classify only the final request field. Other fields are recorded context. Do not execute work.\nDecision: A=extend existing work, B=new independent work, C=explicit timing, D=stop or redirect now, E=park idea, F=ambiguous, G=plan multiple steps, H=app command.\nLane: numbered candidate, 9=outside. Selected is browsing. Related tests extend the matching activity. Resolve references with the latest accepted assignment; corrected replaces previous.\nBoundary: A=now B=after-turn C=after-clip D=when-told.\nCommand: 1=commit 2=push 3=open-pr 4=export 5=branch-from 6=restore 7=stage-mode 8=settings 9=plan-only 10=start-waiting.\nReference example:\n{\"selected\":1,\"tracks\":[{\"lane\":1,\"worker\":\"A\"},{\"lane\":2,\"worker\":\"B\"}],\"recentDecisions\":[{\"corrected\":true,\"previous\":{\"worker\":\"A\"},\"decision\":{\"worker\":\"B\"}}],\"request\":\"Add one more check on that worker.\"}\nDecision: A\nLane: 2\nClassify the current request, not earlier action labels.\n";
const NEW_PREFIX: &str = "The user has explicitly chosen a NEW independent work track. Classify only its start condition. Do not execute work.\nBoundary labels: A=start now, no future condition. B=after the currently RUNNING turn completes. C=after a specific named step completes. D=wait until the USER gives a later explicit go-ahead.\nWaiting for the user's permission or signal is D, never B. Honor negative instructions about starting.\nExamples: Do it now => A. After the running turn finishes => B. After the build step => C. Wait for my go-ahead => D. 合図するまでは始めないで => D.\nClassify only the request field in the JSON below. Previous utterances and decisions are recorded history. Choose only the label.\n";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub id: String,
    pub label: String,
    pub title: String,
    pub status: String,
    pub activity: Option<crate::clip_summaries::Summary>,
    pub running: Option<String>,
    pub planned: Vec<crate::input_context::Planned>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(
    tag = "mode",
    content = "trackId",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum Target {
    #[default]
    None,
    Lock(String),
    New,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "camelCase", deny_unknown_fields)]
pub enum Resolution {
    Retry {
        target: Target,
    },
    Hold,
    Confirm {
        choice: crate::decision_review::Choice,
    },
    Presented,
    StartWaiting {
        choice: crate::app_commands::WaitingRequest,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Extend,
    Fork,
    Schedule,
    Trim,
    Park,
    Ask,
    Plan,
    Command,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub kind: Kind,
    pub track_id: Option<String>,
    pub boundary: Option<String>,
    pub command: Option<String>,
    pub confidence: f64,
    pub inference_ms: f64,
    pub timed_out: bool,
    pub scores: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Timing {
    round_trip_ms: f64,
    inference_ms: f64,
}

impl Decision {
    pub(crate) fn timings(&self) -> Vec<Timing> {
        [self.scores.pointer("/routing/previous"), Some(&self.scores)]
            .into_iter()
            .flatten()
            .filter_map(|value| {
                let valid = |name| {
                    value[name]
                        .as_f64()
                        .filter(|number| number.is_finite() && *number >= 0.0)
                };
                Some(Timing {
                    round_trip_ms: valid("roundTripMs")?,
                    inference_ms: valid("inferenceMs")?,
                })
            })
            .collect()
    }
}

pub struct Request {
    pub wire: Value,
    tracks: Vec<Candidate>,
    target: Target,
    review_text: String,
    calibration: Option<crate::editor_calibration::Applied>,
}

#[derive(Clone, Debug)]
pub struct PendingInput {
    pub id: String,
    pub revision: i64,
    pub text: String,
    pub target: Target,
    pub selected: Option<String>,
    pub tracks: Vec<Candidate>,
    pub expanded_tracks: Vec<Candidate>,
    pub sources: Vec<crate::request_sources::Source>,
    pub has_attachments: bool,
    pub context: Option<String>,
    pub reading_index: i64,
    pub history: crate::input_context::Context,
}

#[derive(Clone)]
pub enum Outcome {
    Rephrase { generate: bool },
    Classified(Decision),
    Asked(String),
    Defer,
}

impl Request {
    pub fn new(
        text: &str,
        tracks: Vec<Candidate>,
        target: Target,
        selected: Option<&str>,
    ) -> Result<Self> {
        if let Target::Lock(id) = &target
            && !tracks.iter().any(|track| &track.id == id)
        {
            return Err(std::io::Error::other("locked track is unavailable").into());
        }
        if tracks.len() > 8 {
            return Err(std::io::Error::other("select at most eight track candidates").into());
        }
        let mut stages = vec![];
        if !matches!(target, Target::New) {
            let labels: Vec<_> = "ABCDEFGH"
                .chars()
                .filter(|&label| label != 'B' || !matches!(target, Target::Lock(_)))
                .map(|label| format!(" {label}"))
                .collect();
            stages.push(json!({"name":"decision","prompt":"\nDecision:","candidates":labels}));
        }
        if matches!(target, Target::None) {
            let mut labels: Vec<_> = (1..=tracks.len()).map(|i| i.to_string()).collect();
            labels.push("9".into());
            stages.push(json!({"name":"lane","prompt":"\nLane: ","candidates":labels,"when":{"decision":[" A"," C"," D"]}}));
        }
        let mut boundary =
            json!({"name":"boundary","prompt":"\nBoundary:","candidates":[" A"," B"," C"," D"]});
        if !matches!(target, Target::New) {
            boundary["when"] = if matches!(target, Target::Lock(_)) {
                json!({"decision":[" C"]})
            } else {
                json!({"decision":[" B"," C"]})
            };
        }
        stages.push(boundary);
        if !matches!(target, Target::New) {
            stages.push(json!({"name":"command","prompt":"\nCommand: ","candidates":(1..=10).map(|n|n.to_string()).collect::<Vec<_>>(),"when":{"decision":[" H"]}}));
        }
        let tail_tracks: Vec<_> = tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut value =
                    json!({"lane":i+1,"worker":t.label,"title":t.title,"state":t.status});
                if let Some(activity) = &t.activity {
                    value["activity"] = json!(activity);
                }
                if let Some(running) = &t.running {
                    value["running"] = json!({"title":running});
                }
                if !t.planned.is_empty() {
                    value["planned"] = json!(t.planned);
                }
                value
            })
            .collect();
        let tail = crate::input_context::serialize_tail(
            &json!({"tracks":tail_tracks,"selected":selected.and_then(|id|tracks.iter().position(|t|t.id==id).map(|i|i+1)),"target":target,"request":text}),
        )?;
        let prefix = if matches!(target, Target::New) {
            NEW_PREFIX
        } else {
            PREFIX
        };
        Ok(Self {
            wire: json!({"prefix":prefix,"tail":tail,"stages":stages,"temperature":1.0,"cachePrefix":false}),
            tracks,
            target,
            review_text: text.to_owned(),
            calibration: None,
        })
    }

    pub fn check_previous_reasons(&mut self) {
        if matches!(self.target, Target::New) {
            return;
        }
        self.wire["stages"].as_array_mut().unwrap().push(json!({"name":"vague-negative","prompt":"\nVague-negative: 1 means the user's final request criticizes previous work without a concrete defect, failed example, or requested correction (for example: still wrong, not right, まだおかしい, 違う). 0 means it gives a specific defect, a new task, or an explicit stop instruction. Choose 0 or 1:","candidates":["0","1"],"when":{"decision":[" A"]}}));
    }

    pub fn calibrate(&mut self, calibration: crate::editor_calibration::Applied) -> Result<()> {
        if !calibration.temperature.is_finite() || !(0.05..=20.0).contains(&calibration.temperature)
        {
            return Err("Invalid calibrated temperature.".into());
        }
        if crate::editor_calibration::Identity::for_prefix(
            self.wire["prefix"]
                .as_str()
                .ok_or("Missing Editor prefix.")?,
            self.wire["cachePrefix"]
                .as_bool()
                .ok_or("Missing Editor loading mode.")?,
        )
        .id()
            != calibration.profile
        {
            return Err("Calibration does not match the Editor request.".into());
        }
        self.wire["temperature"] = json!(calibration.temperature);
        self.calibration = Some(calibration);
        Ok(())
    }

    pub fn reread(&mut self, later: &str) -> Result<()> {
        let mut tail: Value = serde_json::from_str(
            self.wire["tail"]
                .as_str()
                .ok_or("Missing Editor context.")?,
        )?;
        self.review_text = format!(
            "{}\n{later}",
            tail["request"].as_str().ok_or("Missing Editor request.")?
        );
        tail["laterRequest"] = json!({"text":later});
        self.wire["tail"] = crate::input_context::serialize_tail(&tail)?.into();
        let prefix = if matches!(self.target, Target::New) {
            NEW_PREFIX
        } else {
            PREFIX
        };
        self.wire["prefix"] = format!("{prefix}\nThis is a re-reading of a parked request. Use laterRequest as subsequent context to interpret the original request field. Classify the original request, not the later request.\n").into();
        Ok(())
    }

    pub(crate) fn use_screened_context(&mut self, wire: Value) -> Result<()> {
        let tail: Value =
            serde_json::from_str(wire["tail"].as_str().ok_or("Missing Editor context.")?)?;
        let count = tail["tracks"]
            .as_array()
            .ok_or("Missing Editor candidates.")?
            .len();
        if count > self.tracks.len() {
            return Err("Unexpected Editor candidates.".into());
        }
        self.tracks.truncate(count);
        self.wire = wire;
        Ok(())
    }

    pub fn resolve(&self, response: Value) -> Result<Decision> {
        let fail = || std::io::Error::other("invalid Editor decision");
        if response["cachePrefix"] != self.wire["cachePrefix"] {
            return Err(fail().into());
        }
        if let Some(calibration) = &self.calibration {
            let prefix = self.wire["prefix"].as_str().ok_or_else(fail)?;
            if response["temperature"] != json!(calibration.temperature)
                || self.wire["temperature"] != json!(calibration.temperature)
                || crate::editor_calibration::Identity::for_prefix(
                    prefix,
                    self.wire["cachePrefix"].as_bool().ok_or_else(fail)?,
                )
                .id()
                    != calibration.profile
            {
                return Err(fail().into());
            }
        }
        let stages = response["stages"].as_object().ok_or_else(fail)?;
        let definitions = self.wire["stages"].as_array().unwrap();
        let mut selected = BTreeMap::<String, String>::new();
        let mut confidence = 1.0_f64;
        let mut stage_confidence = BTreeMap::new();
        for definition in definitions {
            let name = definition["name"].as_str().unwrap();
            let active = definition
                .get("when")
                .and_then(Value::as_object)
                .is_none_or(|conditions| {
                    conditions.iter().all(|(key, allowed)| {
                        selected.get(key).is_some_and(|chosen| {
                            allowed
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|v| v.as_str() == Some(chosen))
                        })
                    })
                });
            if !active {
                if stages.contains_key(name) {
                    return Err(fail().into());
                }
                continue;
            }
            let stage = stages.get(name).ok_or_else(fail)?;
            let labels = definition["candidates"].as_array().unwrap();
            let logprobs = stage["logprobs"].as_object().ok_or_else(fail)?;
            let probabilities = stage["probabilities"].as_object().ok_or_else(fail)?;
            if labels.len() != logprobs.len() || labels.len() != probabilities.len() {
                return Err(fail().into());
            }
            let choice = stage["selected"].as_str().ok_or_else(fail)?;
            let mut maximum = f64::NEG_INFINITY;
            let mut total = 0.0;
            for label in labels {
                let label = label.as_str().unwrap();
                let log = logprobs
                    .get(label)
                    .and_then(Value::as_f64)
                    .filter(|x| x.is_finite() && *x <= 0.0001)
                    .ok_or_else(fail)?;
                let probability = probabilities
                    .get(label)
                    .and_then(Value::as_f64)
                    .filter(|x| x.is_finite() && (0.0..=1.0).contains(x))
                    .ok_or_else(fail)?;
                maximum = maximum.max(log);
                total += probability;
            }
            let chosen = logprobs
                .get(choice)
                .and_then(Value::as_f64)
                .ok_or_else(fail)?;
            if chosen != maximum || (total - 1.0).abs() > 0.001 {
                return Err(fail().into());
            }
            if self.calibration.is_some() {
                let temperature = self.wire["temperature"].as_f64().ok_or_else(fail)?;
                let normalization: f64 = logprobs
                    .values()
                    .map(|value| ((value.as_f64().unwrap() - maximum) / temperature).exp())
                    .sum();
                for (label, log) in logprobs {
                    let expected =
                        ((log.as_f64().unwrap() - maximum) / temperature).exp() / normalization;
                    if (probabilities[label].as_f64().unwrap() - expected).abs() > 0.000001 {
                        return Err(fail().into());
                    }
                }
            }
            if name != "vague-negative" {
                confidence = confidence.min(probabilities[choice].as_f64().unwrap());
            }
            stage_confidence.insert(name.to_owned(), probabilities[choice].as_f64().unwrap());
            selected.insert(name.into(), choice.into());
        }
        if stages.len() != selected.len() {
            return Err(fail().into());
        }
        let kind = if matches!(self.target, Target::New) {
            Kind::Fork
        } else {
            match selected["decision"].as_str() {
                " A" => Kind::Extend,
                " B" => Kind::Fork,
                " C" => Kind::Schedule,
                " D" => Kind::Trim,
                " E" => Kind::Park,
                " F" => Kind::Ask,
                " G" => Kind::Plan,
                " H" => Kind::Command,
                _ => return Err(fail().into()),
            }
        };
        let track_id = match &self.target {
            Target::Lock(id) => Some(id.clone()),
            _ => selected
                .get("lane")
                .and_then(|n| n.parse::<usize>().ok())
                .and_then(|i| i.checked_sub(1))
                .and_then(|i| self.tracks.get(i))
                .map(|t| t.id.clone()),
        };
        let boundary = match selected.get("boundary").map(String::as_str) {
            Some(" A") => Some("now"),
            Some(" B") => Some("after-turn"),
            Some(" C") => Some("after-clip"),
            Some(" D") => Some("when-told"),
            None => None,
            _ => return Err(fail().into()),
        }
        .map(str::to_owned);
        let command = selected
            .get("command")
            .and_then(|s| s.parse::<usize>().ok())
            .and_then(|n| n.checked_sub(1))
            .and_then(|n| crate::decision_review::COMMANDS.get(n))
            .map(|s| s.to_string());
        let inference_ms = response["inferenceMs"]
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .ok_or_else(fail)?;
        let mut decision = Decision {
            kind,
            track_id,
            boundary,
            command,
            confidence,
            inference_ms,
            timed_out: inference_ms > 1200.0,
            scores: response,
        };
        decision.scores["vagueNegative"] = json!(
            selected
                .get("vague-negative")
                .is_some_and(|choice| choice == "1")
        );
        let model: Value = serde_json::from_str(include_str!(
            "../../../services/editor/src/intercat_editor/model_manifest.json"
        ))?;
        let prefix = self.wire["prefix"].as_str().ok_or_else(fail)?;
        let prefix_hash: String = Sha256::digest(prefix.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let tail: Value = serde_json::from_str(self.wire["tail"].as_str().ok_or_else(fail)?)?;
        let sent_text = tail["request"].as_str().ok_or_else(fail)?;
        let normalized: String = sent_text.nfkc().flat_map(char::to_lowercase).collect();
        let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
        let request_hash: String = Sha256::digest(normalized.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let labels: BTreeMap<String, String> = self
            .tracks
            .iter()
            .enumerate()
            .map(|(index, track)| ((index + 1).to_string(), track.id.clone()))
            .collect();
        decision.scores["calibration"] = json!({
            "id":uuid::Uuid::now_v7(),"backend":"GlimmerLogits(local)",
            "modelRevision":model["revision"],"promptVersion":prefix_hash,
            "cachePrefix":self.wire["cachePrefix"],
            "temperature":self.wire["temperature"],"labelTracks":labels,"target":self.target,"requestHash":request_hash,
        });
        if let Some(calibration) = &self.calibration {
            decision.scores["calibration"]["fit"] = json!(calibration);
        }
        decision.scores["stageConfidence"] = json!(stage_confidence);
        crate::decision_review::gate(&mut decision, &stage_confidence, &self.review_text)?;
        Ok(decision)
    }
}
