//! SSH tunnels: a port on 127.0.0.1 whose connections are forwarded through
//! an SSH server to the database's host and port, as `ssh -L` does.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use dbman_core::{
    Capabilities, Connection, ConnectionConfig, DbError, DbKind, DbObject, DbResult, ObjectPath, SshAuth,
    StatementOutcome, TableDetails,
};
use russh::client;
use russh::keys::{PrivateKeyWithHashAlg, PublicKey};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// How long to wait for the SSH server to answer.
const TIMEOUT: Duration = Duration::from_secs(20);

/// Host keys of SSH servers seen for the first time are kept here.
static KNOWN_HOSTS: OnceLock<PathBuf> = OnceLock::new();

/// Sets DBMan's own known-hosts file. Keys of servers missing from it and from
/// `~/.ssh/known_hosts` are added to it on first use; a key that differs from
/// either file is refused.
pub fn set_known_hosts_file(path: PathBuf) {
    let _ = KNOWN_HOSTS.set(path);
}

/// An open tunnel; it closes when the last holder drops it.
pub struct Tunnel {
    /// The local port that leads to the database.
    pub port: u16,
    handle: Arc<client::Handle<Client>>,
    accept: JoinHandle<()>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

/// Live tunnels by what they lead to, so the sessions of one connection share one SSH login.
static TUNNELS: Mutex<Vec<(String, Weak<Tunnel>)>> = Mutex::new(Vec::new());

struct Client {
    host: String,
    port: u16,
    /// Why the server's key was refused, for the error message.
    problem: Arc<Mutex<Option<String>>>,
}

fn user_known_hosts() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".ssh").join("known_hosts"))
}

/// Whether `key` may be trusted for `host`: known, or new and then remembered.
fn verify(host: &str, port: u16, key: &PublicKey) -> Result<(), String> {
    let changed = |path: &PathBuf, line: usize| {
        format!(
            "The host key of {host} is not the one recorded in {} (line {line}). If the server was \
             reinstalled, remove that line; otherwise someone may be intercepting the connection.",
            path.display()
        )
    };
    let ours = KNOWN_HOSTS.get();
    for path in user_known_hosts().iter().chain(ours) {
        match russh::keys::check_known_hosts_path(host, port, key, path) {
            Ok(true) => return Ok(()),
            Err(russh::keys::Error::KeyChanged { line }) => return Err(changed(path, line)),
            // Unknown here, or no such file.
            _ => {}
        }
    }
    if let Some(path) = ours {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        russh::keys::known_hosts::learn_known_hosts_path(host, port, key, path)
            .map_err(|e| format!("Could not remember the host key of {host}: {e}"))?;
    }
    Ok(())
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &russh::keys::PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            *self.problem.lock().unwrap() = Some("SSH host certificates are not supported.".into());
            return Ok(false);
        };
        match verify(&self.host, self.port, key) {
            Ok(()) => Ok(true),
            Err(e) => {
                *self.problem.lock().unwrap() = Some(e);
                Ok(false)
            }
        }
    }
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => user_known_hosts()
            .and_then(|p| p.parent().and_then(|ssh| ssh.parent()).map(|home| home.join(rest)))
            .unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

fn key_of(cfg: &ConnectionConfig) -> String {
    let s = &cfg.ssh;
    format!("{:?}|{}|{}|{}|{}|{}|{}:{}", s.auth, s.host, s.port, s.user, s.password, s.key_file, cfg.host, cfg.port)
}

/// A tunnel to `cfg`'s database through its SSH server, shared with the
/// other sessions of the same connection while it is open.
pub async fn tunnel(cfg: &ConnectionConfig) -> DbResult<Arc<Tunnel>> {
    let key = key_of(cfg);
    {
        let mut live = TUNNELS.lock().unwrap();
        live.retain(|(_, t)| t.upgrade().is_some_and(|t| !t.handle.is_closed()));
        if let Some(t) = live.iter().find(|(k, _)| *k == key).and_then(|(_, t)| t.upgrade()) {
            return Ok(t);
        }
    }
    let t = Arc::new(open(cfg).await?);
    TUNNELS.lock().unwrap().push((key, Arc::downgrade(&t)));
    Ok(t)
}

async fn open(cfg: &ConnectionConfig) -> DbResult<Tunnel> {
    let ssh = &cfg.ssh;
    let err = |m: String| DbError::new(format!("SSH {}: {m}", ssh.host));
    if ssh.host.trim().is_empty() {
        return Err(DbError::new("The SSH host is empty."));
    }
    if ssh.user.trim().is_empty() {
        return Err(DbError::new("The SSH user is empty."));
    }
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        inactivity_timeout: None,
        ..Default::default()
    });
    let problem = Arc::new(Mutex::new(None));
    let handler = Client { host: ssh.host.trim().to_string(), port: ssh.port, problem: problem.clone() };
    let connect = client::connect(config, (ssh.host.trim(), ssh.port), handler);
    let mut handle = match tokio::time::timeout(TIMEOUT, connect).await {
        Err(_) => return Err(err(format!("no answer on port {} within {} s", ssh.port, TIMEOUT.as_secs()))),
        Ok(Err(e)) => {
            return Err(problem.lock().unwrap().take().map(DbError::new).unwrap_or_else(|| err(e.to_string())));
        }
        Ok(Ok(h)) => h,
    };

    let auth = match ssh.auth {
        SshAuth::Password => handle.authenticate_password(ssh.user.trim(), &ssh.password).await,
        SshAuth::Key => {
            let path = expand_home(ssh.key_file.trim());
            let passphrase = Some(ssh.password.as_str()).filter(|p| !p.is_empty());
            let key = russh::keys::load_secret_key(&path, passphrase)
                .map_err(|e| err(format!("could not read the key {}: {e}", path.display())))?;
            let hash = handle.best_supported_rsa_hash().await.map_err(|e| err(e.to_string()))?.flatten();
            handle.authenticate_publickey(ssh.user.trim(), PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await
        }
    }
    .map_err(|e| err(e.to_string()))?;
    if !auth.success() {
        return Err(err(format!("the server did not accept the sign-in of {}", ssh.user.trim())));
    }

    // Check that the SSH server reaches the database, for a clear error now
    // rather than a dropped connection in the driver.
    let target = if cfg.host.trim().is_empty() { "localhost".to_string() } else { cfg.host.trim().to_string() };
    let probe = handle
        .channel_open_direct_tcpip(target.clone(), cfg.port.into(), "127.0.0.1", 0)
        .await
        .map_err(|e| err(format!("could not reach {target}:{} from there: {e}", cfg.port)))?;
    let _ = probe.close().await;

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| err(e.to_string()))?;
    let port = listener.local_addr().map_err(|e| err(e.to_string()))?.port();
    let handle = Arc::new(handle);
    let h = handle.clone();
    let target_port = cfg.port;
    let accept = tokio::spawn(async move {
        while let Ok((mut socket, peer)) = listener.accept().await {
            let (h, target) = (h.clone(), target.clone());
            tokio::spawn(async move {
                let channel = h
                    .channel_open_direct_tcpip(target, target_port.into(), peer.ip().to_string(), peer.port().into())
                    .await;
                // On failure the socket closes and the driver reports it.
                if let Ok(channel) = channel {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                }
            });
        }
    });
    Ok(Tunnel { port, handle, accept })
}

/// `cfg` pointed at its tunnel's local port, and the tunnel to keep open
/// while it is used; `cfg` itself and no tunnel without SSH.
pub async fn route(cfg: &ConnectionConfig) -> DbResult<(ConnectionConfig, Option<Arc<Tunnel>>)> {
    if !cfg.uses_ssh() {
        return Ok((cfg.clone(), None));
    }
    let t = tunnel(cfg).await?;
    let mut local = cfg.clone();
    local.host = "127.0.0.1".into();
    local.port = t.port;
    local.ssh.enabled = false;
    Ok((local, Some(t)))
}

/// A session that keeps its tunnel open.
pub struct Tunneled {
    pub inner: Arc<dyn Connection>,
    pub tunnel: Arc<Tunnel>,
}

#[async_trait]
impl Connection for Tunneled {
    fn kind(&self) -> DbKind {
        self.inner.kind()
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn server_version(&self) -> DbResult<String> {
        self.inner.server_version().await
    }

    async fn databases(&self) -> DbResult<Vec<String>> {
        self.inner.databases().await
    }

    async fn schemas(&self, database: &str) -> DbResult<Vec<String>> {
        self.inner.schemas(database).await
    }

    async fn objects(&self, database: &str, schema: &str) -> DbResult<Vec<DbObject>> {
        self.inner.objects(database, schema).await
    }

    async fn table_details(&self, path: &ObjectPath) -> DbResult<TableDetails> {
        self.inner.table_details(path).await
    }

    async fn ddl(&self, object: &DbObject) -> DbResult<String> {
        self.inner.ddl(object).await
    }

    async fn execute(&self, database: Option<&str>, sql: &str, max_rows: usize) -> DbResult<StatementOutcome> {
        self.inner.execute(database, sql, max_rows).await
    }

    async fn cancel(&self) -> DbResult<()> {
        self.inner.cancel().await
    }

    async fn command(&self, database: Option<&str>, args: &[String]) -> DbResult<dbman_core::redis::Reply> {
        self.inner.command(database, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_is_expanded() {
        let Some(home) = user_known_hosts().and_then(|p| p.parent()?.parent().map(PathBuf::from)) else { return };
        assert_eq!(expand_home("~/.ssh/id_ed25519"), home.join(".ssh/id_ed25519"));
        assert_eq!(expand_home("/keys/id"), PathBuf::from("/keys/id"));
    }
}
