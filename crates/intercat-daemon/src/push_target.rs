//! A Push review records hashes of resolved destinations, never URL credentials.
//! Live transport strings must be resolved again and match before sending.
use crate::Result;
use intercat_git::{RemoteAddress, Transport};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Destination {
    pub fingerprint: String,
    pub display: String,
    pub credentials: bool,
}
impl Destination {
    pub fn capture(raw: &str) -> Result<Self> {
        let address = RemoteAddress::parse(raw)?;
        let display = if matches!(address.transport, Transport::Http | Transport::Https) {
            let mut url = reqwest::Url::parse(raw).map_err(|_| "Invalid HTTP Git destination.")?;
            url.set_password(None)
                .map_err(|_| "Invalid HTTP Git destination.")?;
            url.set_username("")
                .map_err(|_| "Invalid HTTP Git destination.")?;
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        } else {
            address.display
        };
        Ok(Self {
            fingerprint: Sha256::digest(raw.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            display,
            credentials: address.credentials,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Target {
    pub source_ref: String,
    pub sha: String,
    pub remote: String,
    pub target_ref: String,
    pub destinations: Vec<Destination>,
}
impl Target {
    pub fn validate(&self) -> Result<()> {
        intercat_git::validate_branch_reference(&self.source_ref)?;
        intercat_git::validate_branch_reference(&self.target_ref)?;
        self.sha.parse::<intercat_git::ObjectId>()?;
        validate_remote(&self.remote)?;
        if self.destinations.is_empty() || self.destinations.len() > 16 {
            return Err("Push needs between one and sixteen destinations.".into());
        }
        Ok(())
    }
    pub fn verify_destinations(
        &self,
        remote: &str,
        target_ref: &str,
        raw: &[String],
    ) -> Result<()> {
        self.validate()?;
        let mut expected = self.destinations.clone();
        expected.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
        let mut actual = raw
            .iter()
            .map(|url| Destination::capture(url))
            .collect::<Result<Vec<_>>>()?;
        actual.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
        if self.remote != remote || self.target_ref != target_ref || actual != expected {
            return Err("Destination changed since you reviewed · Review".into());
        }
        Ok(())
    }
    pub fn refspec(&self) -> Result<String> {
        self.validate()?;
        Ok(format!("{}:{}", self.sha, self.target_ref))
    }
}
pub(crate) fn validate_remote(remote: &str) -> Result<()> {
    if remote.is_empty()
        || remote.len() > 256
        || remote.starts_with('-')
        || remote.chars().any(char::is_control)
    {
        return Err("Choose a valid Git remote.".into());
    }
    Ok(())
}
