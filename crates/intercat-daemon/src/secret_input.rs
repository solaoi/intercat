//! A value exists only while preparing one host's environment. Durable records
//! contain the target, variable name and already registered fingerprint.
use crate::Result;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Owner {
    Work,
    Repair { run_id: Uuid },
}
impl Owner {
    pub fn key(&self, track: &str) -> String {
        match self {
            Self::Work => format!("work:{track}"),
            Self::Repair { run_id } => format!("repair:{run_id}"),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Question {
    Msp { id: String, revision: String },
    Report { id: String, revision: i64 },
}
impl Question {
    fn valid(&self) -> bool {
        match self {
            Self::Msp { id, revision } => {
                !id.is_empty()
                    && id.len() <= 128
                    && revision.len() == 64
                    && revision.bytes().all(|b| b.is_ascii_hexdigit())
            }
            Self::Report { id, revision } => Uuid::parse_str(id).is_ok() && *revision > 0,
        }
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub op_id: Uuid,
    pub owner: Owner,
    pub track_id: String,
    pub session_id: String,
    pub turn_id: Uuid,
    pub question: Question,
    pub name: String,
    pub value: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub op_id: Uuid,
    pub owner: Owner,
    pub track_id: String,
    pub session_id: String,
    pub turn_id: Uuid,
    pub question: Question,
    pub name: String,
    pub byte_len: usize,
    pub tag: Vec<u8>,
}
#[derive(Clone)]
pub struct Value {
    name: String,
    bytes: Vec<u8>,
}
impl Value {
    pub fn new(name: String, value: String) -> Result<Self> {
        if !crate::secrets::valid_name(&name)
            || value.is_empty()
            || value.len() > 8192
            || value.contains('\0')
        {
            return Err("Invalid worker secret.".into());
        }
        Ok(Self {
            name,
            bytes: value.into_bytes(),
        })
    }
    pub fn apply(&self, command: &mut tokio::process::Command) {
        use std::os::unix::ffi::OsStrExt;
        command.env(&self.name, std::ffi::OsStr::from_bytes(&self.bytes));
    }
}
impl Drop for Value {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

pub struct Install {
    pub target: Target,
    pub value: Value,
    pub stop_generation: u64,
}
impl Request {
    pub fn prepare(self, key: &crate::secrets::Key) -> Result<Install> {
        if self.op_id.get_version_num() != 7
            || self.track_id.is_empty()
            || self.track_id.len() > 128
            || self.session_id.is_empty()
            || self.session_id.len() > 128
            || !self.question.valid()
        {
            return Err("Invalid secret destination.".into());
        }
        let target = Target {
            op_id: self.op_id,
            owner: self.owner,
            track_id: self.track_id,
            session_id: self.session_id,
            turn_id: self.turn_id,
            question: self.question,
            name: self.name.clone(),
            byte_len: self.value.len(),
            tag: key.tag(self.value.as_bytes()).to_vec(),
        };
        Ok(Install {
            target,
            value: Value::new(self.name, self.value)?,
            stop_generation: 0,
        })
    }
}
