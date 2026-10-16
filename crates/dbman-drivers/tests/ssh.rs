//! SSH tunnels against an SSH server run inside the test: password and key
//! sign-in, host key checks, and forwarding to a TCP echo service. With
//! `DBMAN_TEST_POSTGRES`, `DBMAN_TEST_MYSQL`, `DBMAN_TEST_MSSQL` and
//! `DBMAN_TEST_CLICKHOUSE` set, each server is also queried through a tunnel.

use std::sync::{Arc, Mutex};

use dbman_core::{Cell, ConnectionConfig, DbKind, SshAuth, StatementOutcome};
use dbman_drivers::ssh;
use russh::keys::ssh_key::LineEnding;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{Auth, Msg, Server as _, Session};
use russh::{Channel, ChannelOpenFailure};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const USER: &str = "tunnel";
const PASSWORD: &str = "secret";

#[derive(Clone)]
struct Server {
    client_key: PublicKey,
    /// Where clients asked to be forwarded.
    targets: Arc<Mutex<Vec<String>>>,
}

impl russh::server::Server for Server {
    type Handler = Self;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        self.clone()
    }
}

impl russh::server::Handler for Server {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        Ok(if user == USER && password == PASSWORD { Auth::Accept } else { Auth::reject() })
    }

    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        Ok(if user == USER && key.key_data() == self.client_key.key_data() { Auth::Accept } else { Auth::reject() })
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host: &str,
        port: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.targets.lock().unwrap().push(format!("{host}:{port}"));
        match TcpStream::connect((host, port as u16)).await {
            Ok(mut tcp) => {
                reply.accept().await;
                tokio::spawn(async move {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
                });
            }
            Err(_) => reply.reject(ChannelOpenFailure::ConnectFailed).await,
        }
        Ok(())
    }
}

/// Starts an SSH server; returns its port and its host key.
async fn ssh_server(client_key: PublicKey) -> (u16, PrivateKey, Arc<Mutex<Vec<String>>>) {
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let config = Arc::new(russh::server::Config {
        keys: vec![host_key.clone()],
        auth_rejection_time: std::time::Duration::from_millis(10),
        auth_rejection_time_initial: Some(std::time::Duration::ZERO),
        ..Default::default()
    });
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = socket.local_addr().unwrap().port();
    let targets = Arc::new(Mutex::new(Vec::new()));
    let mut server = Server { client_key, targets: targets.clone() };
    tokio::spawn(async move {
        let _ = server.run_on_socket(config, &socket).await;
    });
    (port, host_key, targets)
}

/// A TCP service that sends back what it gets.
async fn echo() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = l.accept().await {
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    port
}

fn config(ssh_port: u16, target_port: u16) -> ConnectionConfig {
    let mut cfg = ConnectionConfig::new(DbKind::Postgres);
    cfg.host = "127.0.0.1".into();
    cfg.port = target_port;
    cfg.ssh.enabled = true;
    cfg.ssh.host = "127.0.0.1".into();
    cfg.ssh.port = ssh_port;
    cfg.ssh.user = USER.into();
    cfg.ssh.password = PASSWORD.into();
    cfg
}

async fn round_trip(port: u16, text: &[u8]) -> Vec<u8> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(text).await.unwrap();
    let mut back = vec![0; text.len()];
    s.read_exact(&mut back).await.unwrap();
    back
}

#[tokio::test]
async fn tunnels_with_password_and_key() {
    let dir = tempfile::tempdir().unwrap();
    let known = dir.path().join("known_hosts");
    ssh::set_known_hosts_file(known.clone());

    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let (port, host_key, targets) = ssh_server(client_key.public_key().clone()).await;
    let echo_port = echo().await;

    // Password sign-in; the host key is learned on first use.
    let cfg = config(port, echo_port);
    let t = ssh::tunnel(&cfg).await.unwrap();
    assert_eq!(round_trip(t.port, b"hello").await, b"hello");
    assert_eq!(round_trip(t.port, b"again").await, b"again", "every connection gets its own channel");
    let learned = std::fs::read_to_string(&known).unwrap();
    assert!(learned.contains(&format!("[127.0.0.1]:{port}")), "{learned}");
    assert!(targets.lock().unwrap().iter().all(|t| *t == format!("127.0.0.1:{echo_port}")));
    // Sessions of the same connection share the tunnel.
    assert_eq!(ssh::tunnel(&cfg).await.unwrap().port, t.port);
    drop(t);

    // A private key with a passphrase.
    let encrypted = client_key.encrypt(&mut rand::rng(), "pass phrase").unwrap();
    let key_file = dir.path().join("id_ed25519");
    std::fs::write(&key_file, encrypted.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
    let mut by_key = config(port, echo_port);
    by_key.ssh.auth = SshAuth::Key;
    by_key.ssh.key_file = key_file.to_string_lossy().into_owned();
    by_key.ssh.password = "pass phrase".into();
    let t = ssh::tunnel(&by_key).await.unwrap();
    assert_eq!(round_trip(t.port, b"key").await, b"key");

    // Refusals say why.
    let mut wrong = config(port, echo_port);
    wrong.ssh.password = "nope".into();
    let e = ssh::tunnel(&wrong).await.err().unwrap();
    assert!(e.message.contains("did not accept the sign-in"), "{e}");
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
    let e = ssh::tunnel(&config(port, closed)).await.err().unwrap();
    assert!(e.message.contains("could not reach"), "{e}");

    // A server whose key differs from the recorded one is refused.
    let (other, _, _) = ssh_server(client_key.public_key().clone()).await;
    let line = format!("[127.0.0.1]:{other} {}\n", host_key.public_key().to_openssh().unwrap());
    std::fs::write(&known, learned + &line).unwrap();
    let e = ssh::tunnel(&config(other, echo_port)).await.err().unwrap();
    assert!(e.message.contains("not the one recorded"), "{e}");
}

fn from_url(kind: DbKind, var: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(var).ok().filter(|u| !u.is_empty())?;
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(&url);
    let (auth, addr) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, password) = auth.split_once(':').unwrap_or((auth, ""));
    let (hostport, db) = addr.split_once('/').unwrap_or((addr, ""));
    let (host, port) = hostport.split_once(':').unwrap_or((hostport, ""));
    let mut cfg = ConnectionConfig::new(kind);
    cfg.host = host.into();
    if let Ok(p) = port.parse() {
        cfg.port = p;
    }
    cfg.user = user.into();
    cfg.password = password.into();
    cfg.database = db.into();
    cfg.ssl_mode = dbman_core::SslMode::Disable;
    Some(cfg)
}

#[tokio::test]
async fn servers_through_a_tunnel() {
    let client_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap();
    let (port, _, targets) = ssh_server(client_key.public_key().clone()).await;
    for (kind, var) in [
        (DbKind::Postgres, "DBMAN_TEST_POSTGRES"),
        (DbKind::MySql, "DBMAN_TEST_MYSQL"),
        (DbKind::MsSql, "DBMAN_TEST_MSSQL"),
        (DbKind::ClickHouse, "DBMAN_TEST_CLICKHOUSE"),
    ] {
        let Some(mut cfg) = from_url(kind, var) else {
            eprintln!("{var} not set, skipping");
            continue;
        };
        cfg.ssh = config(port, 0).ssh;
        let before = targets.lock().unwrap().len();
        let c = dbman_drivers::connect(&cfg).await.unwrap_or_else(|e| panic!("{kind:?}: {e}"));
        let db = cfg.database.clone();
        match c.execute(Some(&db), "SELECT 41 + 1", 10).await.unwrap() {
            StatementOutcome::Rows(r) => assert_eq!(r.rows[0][0], Cell::Text("42".into()), "{kind:?}"),
            other => panic!("{kind:?}: {other:?}"),
        }
        assert!(targets.lock().unwrap().len() > before, "{kind:?} did not go through the tunnel");
    }
}
