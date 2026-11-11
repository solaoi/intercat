//! One reviewed SSH peer, shared by Git and LFS authentication.
use crate::{
    Result,
    git_tool::GitTool,
    push_auth::{self, Authentication},
    push_broker::Tunnel,
    push_inspect::Inspector,
    push_ssh_config::Configuration,
    push_transfer::Snapshot,
};
use std::path::Path;
use tokio::sync::watch;
pub(crate) struct Access {
    configuration: Configuration,
    pub(crate) tunnel: Tunnel,
    pub(crate) command: String,
}
fn quote(path: &Path) -> Result<String> {
    Ok(serde_json::to_string(
        path.to_str().ok_or("SSH needs UTF-8 paths.")?,
    )?)
}
impl Access {
    pub(crate) async fn open(
        snapshot: &Snapshot,
        git: &GitTool,
        url: &str,
        auth: &mut Authentication,
        cancel: watch::Receiver<bool>,
    ) -> Result<Self> {
        let inspector = Inspector::new(
            git.clone(),
            auth.home.clone(),
            auth.xdg.clone(),
            auth.system,
        )?;
        let configuration =
            Configuration::inspect(&inspector, &snapshot.member().folder, url, cancel).await?;
        let binding = configuration.binding(url)?;
        if !snapshot.member().ssh.contains(&binding) {
            return Err("SSH destination changed since you reviewed · Review".into());
        }
        auth.agent = match configuration.agent.as_deref() {
            Some("none") => None,
            None | Some("SSH_AUTH_SOCK" | "$SSH_AUTH_SOCK") => auth.agent.take(),
            Some(path) => {
                let path = path
                    .strip_prefix("~/")
                    .map(|tail| auth.home.join(tail))
                    .unwrap_or_else(|| path.into());
                Some(push_auth::agent_socket(&snapshot.member().folder, &path)?)
            }
        };
        let tunnel = Tunnel::start(&configuration.host, configuration.port).await?;
        let proxy = shlex::try_join([
            "/usr/bin/nc",
            "-U",
            tunnel
                .socket()
                .to_str()
                .ok_or("SSH needs a UTF-8 tunnel path.")?,
        ])
        .map_err(|_| "Invalid SSH tunnel path.")?;
        let mut args = vec![
            "/usr/bin/ssh".to_owned(),
            "-T".into(),
            "-F".into(),
            configuration
                .file
                .to_str()
                .ok_or("SSH needs a UTF-8 configuration path.")?
                .into(),
        ];
        let mut options = vec![
            "BatchMode=yes".to_owned(),
            "StrictHostKeyChecking=yes".into(),
            "CanonicalizeHostname=no".into(),
            format!("ProxyCommand={proxy}"),
            "ControlMaster=no".into(),
            "ControlPath=none".into(),
            "ControlPersist=no".into(),
            "ForwardAgent=no".into(),
            "ForwardX11=no".into(),
            "ClearAllForwardings=yes".into(),
            "PermitLocalCommand=no".into(),
            "RemoteCommand=none".into(),
            "AddKeysToAgent=no".into(),
            "UpdateHostKeys=no".into(),
            "StdinNull=no".into(),
            format!("HostName={}", configuration.host),
            format!("Port={}", configuration.port),
            format!("User={}", configuration.user),
        ];
        if !auth.system {
            options.extend([
                "GlobalKnownHostsFile=/dev/null".into(),
                format!(
                    "UserKnownHostsFile={}",
                    quote(&auth.home.join(".ssh/known_hosts"))?
                ),
            ]);
        }
        options.push(match &auth.agent {
            Some(agent) => format!("IdentityAgent={}", quote(agent)?),
            None => "IdentityAgent=none".into(),
        });
        for option in options {
            args.extend(["-o".into(), option]);
        }
        let command = shlex::try_join(args.iter().map(String::as_str))
            .map_err(|_| "Invalid SSH command arguments.")?;
        Ok(Self {
            configuration,
            tunnel,
            command,
        })
    }
    pub(crate) fn root(&self) -> &Path {
        self.configuration.root()
    }
}
