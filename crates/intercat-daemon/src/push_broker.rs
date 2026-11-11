//! Git's private Unix broker connects to one reviewed peer. The pinned LFS
//! client gets a separate authenticated loopback proxy for service-provided
//! storage locations. DNS and TCP remain in these bounded brokers.
use crate::Result;
use ring::rand::{SecureRandom, SystemRandom};
use std::{
    fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UnixListener},
    task::{JoinHandle, JoinSet},
};

pub struct Broker {
    forwarder: Forwarder,
    password: String,
}
pub struct Tunnel {
    forwarder: Forwarder,
}
struct Forwarder {
    root: tempfile::TempDir,
    socket: PathBuf,
    task: JoinHandle<()>,
}
enum Scope {
    ReviewedPeer,
    LfsService,
}
enum Protocol {
    Socks5(String, Scope),
    Stream,
}
struct Peer {
    host: String,
    port: u16,
    addresses: Vec<SocketAddr>,
    protocol: Protocol,
}
impl Broker {
    pub async fn start(host: &str, port: u16) -> Result<Self> {
        let mut random = [0; 32];
        SystemRandom::new()
            .fill(&mut random)
            .map_err(|_| "The private Push connection could not be created.")?;
        let password = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let forwarder = Forwarder::start(
            host,
            port,
            Protocol::Socks5(password.clone(), Scope::ReviewedPeer),
        )
        .await?;
        Ok(Self {
            forwarder,
            password,
        })
    }
    pub fn socket(&self) -> &Path {
        &self.forwarder.socket
    }
    pub fn root(&self) -> &Path {
        self.forwarder.root.path()
    }
    pub fn proxy_url(&self) -> String {
        format!(
            "socks5h://intercat:{}@localhost{}",
            self.password,
            self.socket().display()
        )
    }
}
impl Tunnel {
    pub async fn start(host: &str, port: u16) -> Result<Self> {
        Ok(Self {
            forwarder: Forwarder::start(host, port, Protocol::Stream).await?,
        })
    }
    pub fn socket(&self) -> &Path {
        &self.forwarder.socket
    }
    pub fn root(&self) -> &Path {
        self.forwarder.root.path()
    }
}
impl Forwarder {
    async fn start(host: &str, port: u16, protocol: Protocol) -> Result<Self> {
        let peer = resolved_peer(host, port, protocol).await?;
        let root = tempfile::Builder::new()
            .prefix("ic-push-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in("/private/tmp")?;
        let socket = root.path().join("s");
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{
                        let Ok((mut client,_))=accepted else {break;};
                        if connections.len()>=16 {continue;}
                        let peer=peer.clone();
                        connections.spawn(async move {
                            let connected=tokio::time::timeout(Duration::from_secs(10),connect(&mut client,&peer)).await;
                            if let Ok(Ok(mut remote))=connected {
                                let _=tokio::time::timeout(Duration::from_secs(150),tokio::io::copy_bidirectional(&mut client,&mut remote)).await;
                            }
                        });
                    }
                    _=connections.join_next(),if !connections.is_empty()=>{}
                }
            }
        });
        Ok(Self { root, socket, task })
    }
}
impl Drop for Forwarder {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn resolved_peer(host: &str, port: u16, protocol: Protocol) -> Result<std::sync::Arc<Peer>> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if matches!(protocol, Protocol::Socks5(_, Scope::LfsService)) {
        return Ok(std::sync::Arc::new(Peer {
            host: host.into(),
            port,
            addresses: Vec::new(),
            protocol,
        }));
    }
    let mut addresses = if host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
        || host
            .trim_end_matches('.')
            .to_ascii_lowercase()
            .ends_with(".localhost")
    {
        // RFC 6761: localhost names denote loopback and never require DNS.
        vec![
            SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
            SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port),
        ]
    } else {
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio::net::lookup_host((host, port)),
        )
        .await
        .map_err(|_| "The Push destination lookup timed out.")??
        .collect::<Vec<_>>()
    };
    addresses.sort();
    addresses.dedup();
    if addresses.is_empty() || addresses.len() > 32 {
        return Err("The Push destination could not be resolved safely.".into());
    }
    Ok(std::sync::Arc::new(Peer {
        host: host.into(),
        port,
        addresses,
        protocol,
    }))
}

/// git-lfs uses Go's proxy transport, which requires a TCP SOCKS endpoint.
/// The sandbox permits only this loopback port. Only the trusted, pinned LFS
/// client and user authentication helpers receive this capability, allowing
/// storage locations returned by the approved LFS service (human decision).
pub struct TcpBroker {
    port: u16,
    password: String,
    task: JoinHandle<()>,
}
impl TcpBroker {
    pub async fn for_lfs_service(host: &str, port: u16) -> Result<Self> {
        let mut random = [0; 32];
        SystemRandom::new()
            .fill(&mut random)
            .map_err(|_| "The private LFS connection could not be created.")?;
        let password = random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let peer = resolved_peer(
            host,
            port,
            Protocol::Socks5(password.clone(), Scope::LfsService),
        )
        .await?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{
                        let Ok((mut client,_))=accepted else {break;};
                        if connections.len()>=16 {continue;}
                        let peer=peer.clone();
                        connections.spawn(async move {
                            let connected=tokio::time::timeout(Duration::from_secs(10),connect(&mut client,&peer)).await;
                            if let Ok(Ok(mut remote))=connected {
                                let _=tokio::time::timeout(Duration::from_secs(150),tokio::io::copy_bidirectional(&mut client,&mut remote)).await;
                            }
                        });
                    }
                    _=connections.join_next(),if !connections.is_empty()=>{}
                }
            }
        });
        Ok(Self {
            port,
            password,
            task,
        })
    }
    pub fn port(&self) -> u16 {
        self.port
    }
    pub fn proxy_url(&self) -> String {
        format!(
            "socks5h://intercat:{}@127.0.0.1:{}",
            self.password, self.port
        )
    }
}
impl Drop for TcpBroker {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn bytes<S: AsyncRead + Unpin>(client: &mut S, count: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0; count];
    client.read_exact(&mut bytes).await?;
    Ok(bytes)
}
async fn reject<S: AsyncWrite + Unpin>(client: &mut S) -> Result<TcpStream> {
    client.write_all(&[5, 2, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    Err("The private Push connection requested another destination.".into())
}
async fn connect<S: AsyncRead + AsyncWrite + Unpin>(
    client: &mut S,
    peer: &Peer,
) -> Result<TcpStream> {
    let (expected_password, scope) = match &peer.protocol {
        Protocol::Stream => return Ok(TcpStream::connect(peer.addresses.as_slice()).await?),
        Protocol::Socks5(password, scope) => (password, scope),
    };
    let greeting = bytes(client, 2).await?;
    if greeting[0] != 5 || greeting[1] == 0 || greeting[1] > 16 {
        return Err("Invalid private Push handshake.".into());
    }
    let methods = bytes(client, usize::from(greeting[1])).await?;
    if !methods.contains(&2) {
        client.write_all(&[5, 255]).await?;
        return Err("Private Push authentication is required.".into());
    }
    client.write_all(&[5, 2]).await?;
    let auth = bytes(client, 2).await?;
    if auth[0] != 1 {
        return Err("Invalid private Push authentication.".into());
    }
    let user = bytes(client, usize::from(auth[1])).await?;
    let length = client.read_u8().await?;
    let password = bytes(client, usize::from(length)).await?;
    if !bool::from(user.as_slice().ct_eq(b"intercat"))
        || !bool::from(password.as_slice().ct_eq(expected_password.as_bytes()))
    {
        client.write_all(&[1, 1]).await?;
        return Err("Private Push authentication failed.".into());
    }
    client.write_all(&[1, 0]).await?;
    let request = bytes(client, 4).await?;
    if request[..3] != [5, 1, 0] {
        return reject(client).await;
    }
    let host = match request[3] {
        1 => {
            let value = bytes(client, 4).await?;
            Ipv4Addr::new(value[0], value[1], value[2], value[3]).to_string()
        }
        3 => {
            let length = client.read_u8().await?;
            String::from_utf8(bytes(client, usize::from(length)).await?)
                .map_err(|_| "Invalid private Push host.")?
        }
        4 => {
            let value: [u8; 16] = bytes(client, 16)
                .await?
                .try_into()
                .map_err(|_| "Invalid private Push host.")?;
            Ipv6Addr::from(value).to_string()
        }
        _ => return reject(client).await,
    };
    let port = client.read_u16().await?;
    if host.is_empty() || host.chars().any(char::is_control) || port == 0 {
        return reject(client).await;
    }
    if matches!(scope, Scope::LfsService) {
        // This endpoint is selected by the pinned LFS client's standard API
        // flow. Project code/configured transfer adapters never run here.
        let resolved = resolved_peer(&host, port, Protocol::Stream).await?;
        let remote = TcpStream::connect(resolved.addresses.as_slice()).await?;
        client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Ok(remote);
    }
    let same_host = host
        .trim_end_matches('.')
        .eq_ignore_ascii_case(peer.host.trim_end_matches('.'))
        || host
            .parse::<IpAddr>()
            .is_ok_and(|ip| peer.addresses.iter().any(|address| address.ip() == ip));
    if port != peer.port || !same_host {
        return reject(client).await;
    }
    let remote = TcpStream::connect(peer.addresses.as_slice()).await?;
    client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    Ok(remote)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixStream;
    async fn authenticate(broker: &Broker, password: &str) -> Result<UnixStream> {
        let mut client = UnixStream::connect(broker.socket()).await?;
        client.write_all(&[5, 1, 2]).await?;
        assert_eq!(bytes(&mut client, 2).await?, [5, 2]);
        let mut input = vec![1, 8];
        input.extend_from_slice(b"intercat");
        input.push(u8::try_from(password.len())?);
        input.extend_from_slice(password.as_bytes());
        client.write_all(&input).await?;
        let reply = bytes(&mut client, 2).await?;
        if password != broker.password {
            assert_eq!(reply, [1, 1]);
            return Ok(client);
        }
        assert_eq!(reply, [1, 0]);
        Ok(client)
    }
    async fn request<S: AsyncRead + AsyncWrite + Unpin>(
        client: &mut S,
        host: &str,
        port: u16,
    ) -> Result<Vec<u8>> {
        let mut input = vec![5, 1, 0, 3, u8::try_from(host.len())?];
        input.extend_from_slice(host.as_bytes());
        input.extend_from_slice(&port.to_be_bytes());
        client.write_all(&input).await?;
        bytes(client, 10).await
    }
    #[tokio::test]
    async fn private_broker_rejects_wrong_auth_host_port_and_closes_owned_connections() -> Result<()>
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let broker = Broker::start("127.0.0.1", port).await?;
        let mut anonymous = UnixStream::connect(broker.socket()).await?;
        anonymous.write_all(&[5, 1, 0]).await?;
        assert_eq!(bytes(&mut anonymous, 2).await?, [5, 255]);
        let _wrong = authenticate(&broker, "DUMMY_WRONG_PASSWORD").await?;
        for (host, requested_port) in [
            ("unapproved.example.invalid", port),
            ("127.0.0.1", port.wrapping_add(1)),
        ] {
            let mut client = authenticate(&broker, &broker.password).await?;
            assert_eq!(request(&mut client, host, requested_port).await?[1], 2);
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        }
        let mut client = authenticate(&broker, &broker.password).await?;
        assert_eq!(request(&mut client, "127.0.0.1", port).await?[1], 0);
        let (server, _) = listener.accept()?;
        server.set_nonblocking(true)?;
        let mut server = TcpStream::from_std(server)?;
        client.write_all(b"dummy ping").await?;
        assert_eq!(bytes_tcp(&mut server, 10).await?, b"dummy ping");
        server.write_all(b"dummy pong").await?;
        assert_eq!(bytes(&mut client, 10).await?, b"dummy pong");
        let socket = broker.socket().to_path_buf();
        drop(broker);
        assert!(!socket.exists());
        let mut byte = [0; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), server.read(&mut byte)).await??,
            0
        );
        Ok(())
    }
    async fn bytes_tcp(stream: &mut TcpStream, count: usize) -> Result<Vec<u8>> {
        let mut data = vec![0; count];
        stream.read_exact(&mut data).await?;
        Ok(data)
    }
    #[tokio::test]
    async fn lfs_capability_authenticates_and_relays_service_provided_storage() -> Result<()> {
        let broker = TcpBroker::for_lfs_service("dummy-lfs.localhost", 443).await?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut anonymous = TcpStream::connect((Ipv4Addr::LOCALHOST, broker.port())).await?;
        anonymous.write_all(&[5, 1, 0]).await?;
        assert_eq!(bytes(&mut anonymous, 2).await?, [5, 255]);
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, broker.port())).await?;
        client.write_all(&[5, 1, 2]).await?;
        assert_eq!(bytes(&mut client, 2).await?, [5, 2]);
        let mut auth = vec![1, 8];
        auth.extend_from_slice(b"intercat");
        auth.push(u8::try_from(broker.password.len())?);
        auth.extend_from_slice(broker.password.as_bytes());
        client.write_all(&auth).await?;
        assert_eq!(bytes(&mut client, 2).await?, [1, 0]);
        assert_eq!(
            request(
                &mut client,
                "dummy-storage.localhost",
                listener.local_addr()?.port()
            )
            .await?[1],
            0
        );
        let (mut server, _) = listener.accept().await?;
        client.write_all(b"dummy delegated payload").await?;
        assert_eq!(bytes(&mut server, 23).await?, b"dummy delegated payload");
        drop(broker);
        let mut byte = [0; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), server.read(&mut byte)).await??,
            0
        );
        Ok(())
    }
}
