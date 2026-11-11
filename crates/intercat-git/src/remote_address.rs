//! Interpret Git's URL and SCP forms without invoking a transport helper.
use crate::Result;
use gix::url::{ArgumentSafety, Scheme};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    File,
    Git,
    Ssh,
    Http,
    Https,
}
pub struct RemoteAddress {
    pub transport: Transport,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub explicit_port: Option<u16>,
    pub user: Option<String>,
    pub credentials: bool,
    pub display: String,
}
impl RemoteAddress {
    pub fn parse(raw: &str) -> Result<Self> {
        if raw.is_empty() || raw.len() > 16 * 1024 || raw.chars().any(char::is_control) {
            return Err("Invalid Git destination.".into());
        }
        // Parser errors contain their input, which can include credentials.
        let mut address = gix::url::parse(raw).map_err(|_| "Invalid Git destination.")?;
        let transport = match address.scheme {
            Scheme::File => Transport::File,
            Scheme::Git => Transport::Git,
            Scheme::Ssh => Transport::Ssh,
            Scheme::Http => Transport::Http,
            Scheme::Https => Transport::Https,
            _ => return Err("This Git transport requires an unsupported remote helper.".into()),
        };
        if matches!(address.host_as_argument(), ArgumentSafety::Dangerous(_))
            || matches!(address.user_as_argument(), ArgumentSafety::Dangerous(_))
            || address.path.first() == Some(&b'-')
        {
            return Err("Invalid Git destination argument.".into());
        }
        if address
            .user
            .as_deref()
            .is_some_and(|user| user.chars().any(char::is_control))
        {
            return Err("Invalid Git destination user.".into());
        }
        if transport == Transport::Ssh && address.password.is_some() {
            return Err(
                "SSH Push uses your agent or key. Remove the password from its URL.".into(),
            );
        }
        let credentials = address.user.is_some() || address.password.is_some();
        let host = address.host.clone();
        let port = address.port_or_default();
        let explicit_port = address.port;
        let user = address.user.clone();
        address.set_password(None);
        address.set_user(None);
        Ok(Self {
            transport,
            host,
            port,
            explicit_port,
            user,
            credentials,
            display: address.to_string(),
        })
    }
}

pub fn validate_branch_reference(reference: &str) -> Result<()> {
    if !reference.starts_with("refs/heads/") || reference.len() > 1024 {
        return Err("Push needs one branch reference.".into());
    }
    let _: gix::refs::FullName = reference
        .try_into()
        .map_err(|_| "Invalid branch reference.")?;
    Ok(())
}
