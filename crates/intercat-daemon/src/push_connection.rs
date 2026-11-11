//! Destination-specific transports share only verified wire results.
use crate::{
    Result,
    git_tool::GitTool,
    push_auth::Authentication,
    push_inspect::Inspector,
    push_transfer::Snapshot,
    push_wire::{Advertised, Receipt},
};
use intercat_git::{RemoteAddress, Transport};
use std::{ffi::OsStr, path::Path};
use tokio::sync::watch;
pub(crate) enum Connection<'a> {
    File(crate::push_file::Client<'a>),
    Http(crate::push_http::Client<'a>),
    Ssh(crate::push_ssh::Client<'a>),
}
impl<'a> Connection<'a> {
    pub(crate) async fn open(
        snapshot: &'a Snapshot,
        git: GitTool,
        inspector: &Inspector,
        url: &str,
        path: &OsStr,
        agent: Option<&Path>,
        cancel: watch::Receiver<bool>,
    ) -> Result<Self> {
        match RemoteAddress::parse(url)?.transport {
            Transport::File => Ok(Self::File(crate::push_file::Client::new(
                snapshot, git, url,
            )?)),
            Transport::Http | Transport::Https => {
                let auth = Authentication::capture(
                    inspector,
                    &snapshot.member().folder,
                    &git,
                    url,
                    path,
                    agent,
                    cancel,
                )
                .await?;
                Ok(Self::Http(
                    crate::push_http::Client::new(snapshot, git, url, auth).await?,
                ))
            }
            Transport::Ssh => {
                let auth = Authentication::capture(
                    inspector,
                    &snapshot.member().folder,
                    &git,
                    url,
                    path,
                    agent,
                    cancel.clone(),
                )
                .await?;
                Ok(Self::Ssh(
                    crate::push_ssh::Client::new(snapshot, git, url, auth, cancel).await?,
                ))
            }
            _ => Err("This Push transport is not supported.".into()),
        }
    }
    pub(crate) async fn advertise(&self, cancel: watch::Receiver<bool>) -> Result<Vec<Advertised>> {
        match self {
            Self::File(client) => client.advertise(cancel).await,
            Self::Http(client) => client.advertise(cancel).await,
            Self::Ssh(client) => client.advertise(cancel).await,
        }
    }
    pub(crate) async fn send(&self, cancel: watch::Receiver<bool>) -> Result<Receipt> {
        match self {
            Self::File(client) => client.send(cancel).await,
            Self::Http(client) => client.send(cancel).await,
            Self::Ssh(client) => client.send(cancel).await,
        }
    }
}
