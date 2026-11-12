//! SFTP 백엔드. SSH 연결 하나에 SFTP 채널 여러 개를 열어 병렬 요청을 나눠 처리합니다.

mod known_hosts;
mod ssh_config;

use crate::proxy::Proxy;
use async_trait::async_trait;
use bytes::Bytes;
use expandrv_core::config::{SecretResolver, SftpConfig};
use expandrv_core::{
    Backend, Capabilities, Entry, EntryKind, Error, Quota, RemotePath, Result, WriteSource,
};
use futures::stream::{FuturesUnordered, StreamExt};
use known_hosts::HostKeyCheck;
use russh::client;
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::RawSftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{FileAttributes, OpenFlags, Packet, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt};

/// SSH 연결 하나에 여는 SFTP 채널 수. OpenSSH sftp-server는 채널마다 요청을 순서대로 처리합니다.
const CHANNELS: usize = 4;
/// 한 작업에서 동시에 보내는 읽기/쓰기 요청 수
const MAX_INFLIGHT: usize = 16;
const READ_CHUNK: u64 = 64 * 1024;
/// SFTP v3 서버가 반드시 받아야 하는 최소 쓰기 크기
const WRITE_CHUNK_MIN: u64 = 32 * 1024;
const WRITE_CHUNK_MAX: u64 = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const REQUEST_TIMEOUT_SECS: u64 = 60;
const POSIX_RENAME: &str = "posix-rename@openssh.com";

/// 재연결에 필요한 접속 정보. 비밀 값을 담으므로 `Debug`를 구현하지 않습니다.
struct Target {
    ssh_shell: bool,
    host: String,
    port: u16,
    username: String,
    known_hosts: Vec<PathBuf>,
    password: Option<SecretString>,
    keys: Vec<Arc<PrivateKey>>,
    proxy: Option<Arc<Proxy>>,
}

struct Conn {
    handle: client::Handle<HostKeyHandler>,
    sessions: Vec<RawSftpSession>,
    next: AtomicUsize,
    posix_rename: bool,
    statvfs: bool,
    read_chunk: u64,
    write_chunk: u64,
}

impl Conn {
    /// 작업 하나는 같은 채널을 끝까지 씁니다(파일 핸들이 채널별이므로).
    fn pick(&self) -> &RawSftpSession {
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.sessions.len();
        &self.sessions[i]
    }
}

pub struct SftpBackend {
    target: Target,
    root: String,
    conn: tokio::sync::Mutex<Option<Arc<Conn>>>,
}

impl SftpBackend {
    pub async fn connect(cfg: &SftpConfig, secrets: &dyn SecretResolver) -> Result<SftpBackend> {
        Self::connect_via(cfg, secrets, None).await
    }

    /// 프록시를 거쳐 연결합니다. 재연결도 같은 프록시를 씁니다.
    pub async fn connect_via(
        cfg: &SftpConfig,
        secrets: &dyn SecretResolver,
        proxy: Option<Arc<Proxy>>,
    ) -> Result<SftpBackend> {
        let resolved = if cfg.use_ssh_config {
            Some(ssh_config::resolve(&cfg.host).await?)
        } else {
            None
        };
        let password = cfg
            .password
            .as_ref()
            .map(|s| secrets.resolve(s))
            .transpose()?;
        let passphrase = cfg
            .key_passphrase
            .as_ref()
            .map(|s| secrets.resolve(s))
            .transpose()?;
        let mut keys = Vec::new();
        let mut key_error = None;
        let paths = if let Some(r) = &resolved {
            r.identities.clone()
        } else {
            cfg.private_key.iter().cloned().collect()
        };
        for path in paths {
            if resolved.is_some() && !path.is_file() {
                continue;
            }
            match load_key(&path, passphrase.as_ref()) {
                Ok(key) => keys.push(Arc::new(key)),
                Err(e) if resolved.is_some() => key_error = Some(e),
                Err(e) => return Err(e),
            }
        }
        if password.is_none() && keys.is_empty() {
            if let Some(e) = key_error {
                return Err(e);
            }
            return Err(Error::Config(
                "SFTP 인증 수단이 없습니다. 암호나 개인키를 지정하세요. SSH 설정 사용 시 디스크의 IdentityFile이 필요합니다(ssh-agent 미지원)".into(),
            ));
        }
        let known_hosts = match &cfg.known_hosts {
            Some(p) => vec![p.clone()],
            None => match &resolved {
                Some(r) => r.known_hosts.clone(),
                None => vec![default_known_hosts()?],
            },
        };
        let target = Target {
            ssh_shell: cfg.ssh_shell,
            host: resolved
                .as_ref()
                .map_or_else(|| cfg.host.clone(), |r| r.host.clone()),
            port: resolved.as_ref().map_or(cfg.port, |r| r.port),
            username: resolved
                .as_ref()
                .map_or_else(|| cfg.username.clone(), |r| r.user.clone()),
            known_hosts,
            password,
            keys,
            proxy,
        };

        let conn = Arc::new(open_conn(&target).await?);
        let root = resolve_root(conn.pick(), &cfg.root).await?;
        tracing::info!(host = %target.host, port = target.port, root = %root, "SFTP 연결");
        Ok(Self {
            target,
            root,
            conn: tokio::sync::Mutex::new(Some(conn)),
        })
    }

    fn remote(&self, path: &RemotePath) -> String {
        remote_path(&self.root, path)
    }

    /// 끊긴 연결은 다음 호출에서 다시 엽니다. 잠금을 쥔 채 연결해 동시 재연결을 막습니다.
    async fn conn(&self) -> Result<Arc<Conn>> {
        let mut slot = self.conn.lock().await;
        if let Some(c) = slot.as_ref() {
            if !c.handle.is_closed() {
                return Ok(c.clone());
            }
            tracing::warn!(host = %self.target.host, "SFTP 연결이 끊겨 다시 연결합니다");
        }
        *slot = None;
        let c = Arc::new(open_conn(&self.target).await?);
        *slot = Some(c.clone());
        Ok(c)
    }

    async fn invalidate(&self, conn: &Arc<Conn>) {
        let mut slot = self.conn.lock().await;
        if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, conn)) {
            *slot = None;
        }
    }
}

/// 연결 오류면 연결을 버리고, `$retries`번까지 새 연결로 다시 시도합니다.
macro_rules! with_conn {
    ($self:ident, $retries:expr, |$c:ident| $body:expr) => {{
        let mut left: u8 = $retries;
        loop {
            let $c = $self.conn().await?;
            match $body {
                Err(e) if matches!(e, Error::Network(_) | Error::Timeout) => {
                    $self.invalidate(&$c).await;
                    if left == 0 {
                        break Err(e);
                    }
                    left -= 1;
                    tracing::warn!(error = %e, "SFTP 연결 오류로 재시도합니다");
                }
                r => break r,
            }
        }
    }};
}

#[async_trait]
impl Backend for SftpBackend {
    fn kind(&self) -> &'static str {
        "sftp"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            range_read: true,
            set_mtime: true,
            rename_dir: true,
            case_sensitive: true,
            sequential_transfer: true,
        }
    }

    async fn stat(&self, path: &RemotePath) -> Result<Entry> {
        let p = self.remote(path);
        let name = path.name().unwrap_or("");
        with_conn!(self, 1, |c| stat_entry(c.pick(), &p, name).await)
    }

    async fn list(&self, path: &RemotePath) -> Result<Vec<Entry>> {
        let p = self.remote(path);
        with_conn!(self, 1, |c| list_dir(c.pick(), &p).await)
    }

    async fn read_range(&self, path: &RemotePath, offset: u64, len: u64) -> Result<Bytes> {
        let p = self.remote(path);
        with_conn!(self, 1, |c| read_range(&c, &p, offset, len).await)
    }

    async fn write(&self, path: &RemotePath, source: WriteSource) -> Result<Entry> {
        let p = self.remote(path);
        let name = path.name().unwrap_or("");
        // 임시 파일에 올린 뒤 교체하므로 재시도해도 결과가 같습니다.
        with_conn!(self, 1, |c| write_file(&c, &p, name, &source).await)
    }

    async fn mkdir(&self, path: &RemotePath) -> Result<Entry> {
        let p = self.remote(path);
        let name = path.name().unwrap_or("");
        with_conn!(self, 0, |c| make_dir(c.pick(), &p, name).await)
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> Result<()> {
        let (src, dst) = (self.remote(from), self.remote(to));
        with_conn!(self, 0, |c| rename(&c, &src, &dst, replace).await)
    }

    async fn delete(&self, path: &RemotePath, kind: EntryKind) -> Result<()> {
        let p = self.remote(path);
        with_conn!(self, 0, |c| delete(c.pick(), &p, kind).await)
    }

    async fn set_mtime(&self, path: &RemotePath, mtime: SystemTime) -> Result<()> {
        let p = self.remote(path);
        let secs = unix_secs(mtime);
        with_conn!(self, 1, |c| set_mtime(c.pick(), &p, secs).await)
    }

    async fn quota(&self) -> Result<Quota> {
        with_conn!(self, 1, |c| quota(&c, &self.root).await)
    }
}

// ---------- 연결과 인증 ----------

struct HostKeyHandler {
    host: String,
    port: u16,
    known_hosts: Vec<PathBuf>,
    /// 거부 사유. 연결 실패 후 `Error::Auth` 메시지로 씁니다.
    rejection: Arc<StdMutex<Option<String>>>,
}

impl client::Handler for HostKeyHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = server_key.public_key();
        let fp = key.fingerprint(HashAlg::Sha256);
        let who = format!("{}:{}", self.host, self.port);
        let file = self
            .known_hosts
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let verdict = if server_key.certificate().is_some() {
            Err(format!(
                "호스트 키를 신뢰할 수 없습니다: {who}가 호스트 인증서를 보냈지만 지원하지 않습니다(키 지문 {fp})"
            ))
        } else {
            let mut text = String::new();
            for path in &self.known_hosts {
                match std::fs::read_to_string(path) {
                    Ok(t) => {
                        text.push_str(&t);
                        text.push('\n');
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        *self.rejection.lock().unwrap_or_else(|p| p.into_inner()) = Some(format!(
                            "호스트 키를 확인할 수 없습니다: known_hosts({file})를 읽지 못했습니다: {e}"
                        ));
                        return Ok(false);
                    }
                }
            }
            let algo = key.algorithm();
            match known_hosts::check(&text, &self.host, self.port, &key) {
                HostKeyCheck::Trusted => Ok(()),
                HostKeyCheck::Unknown => Err(format!(
                    "호스트 키를 신뢰할 수 없습니다: {who}의 {algo} 키 {fp}가 known_hosts({file})에 없습니다. 지문을 확인한 뒤 known_hosts에 추가하세요"
                )),
                HostKeyCheck::Mismatch { line } => Err(format!(
                    "호스트 키를 신뢰할 수 없습니다: {who}의 {algo} 키 {fp}가 known_hosts({file}) {line}행 기록과 다릅니다(중간자 공격 가능성)"
                )),
                HostKeyCheck::Revoked { line } => Err(format!(
                    "호스트 키를 신뢰할 수 없습니다: {who}의 {algo} 키 {fp}는 known_hosts({file}) {line}행에서 폐기되었습니다"
                )),
            }
        };
        match verdict {
            Ok(()) => Ok(true),
            Err(msg) => {
                *self.rejection.lock().unwrap_or_else(|p| p.into_inner()) = Some(msg);
                Ok(false)
            }
        }
    }
}

fn default_known_hosts() -> Result<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(std::env::home_dir)
        .map(|home| home.join(".ssh").join("known_hosts"))
        .ok_or_else(|| Error::Config("known_hosts 경로를 정할 수 없습니다".into()))
}

fn load_key(path: &std::path::Path, passphrase: Option<&SecretString>) -> Result<PrivateKey> {
    if !path.is_file() {
        return Err(Error::Config(format!(
            "개인 키 파일 {}이 없습니다",
            path.display()
        )));
    }
    russh::keys::load_secret_key(path, passphrase.map(|p| p.expose_secret()))
        .map_err(|e| Error::Auth(format!("개인 키 {}를 열 수 없습니다: {e}", path.display())))
}

async fn open_conn(t: &Target) -> Result<Conn> {
    let rejection = Arc::new(StdMutex::new(None));
    let handler = HostKeyHandler {
        host: t.host.clone(),
        port: t.port,
        known_hosts: t.known_hosts.clone(),
        rejection: rejection.clone(),
    };
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        nodelay: true,
        ..Default::default()
    });
    let connecting = async {
        match &t.proxy {
            Some(p) => {
                let stream = p
                    .connect(&t.host, t.port)
                    .await
                    .map_err(russh::Error::from)?;
                client::connect_stream(config, stream, handler).await
            }
            None => client::connect(config, (t.host.as_str(), t.port), handler).await,
        }
    };
    let mut handle = match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
        Err(_) => return Err(Error::Timeout),
        Ok(Ok(h)) => h,
        Ok(Err(e)) => {
            let msg = rejection.lock().unwrap_or_else(|p| p.into_inner()).take();
            return Err(match msg {
                Some(msg) => Error::Auth(msg),
                None => map_ssh(e),
            });
        }
    };

    tracing::debug!("SFTP SSH 연결 완료, 인증 시작");
    authenticate(&mut handle, t).await?;
    tracing::debug!("SFTP 인증 완료, 채널 준비 시작");

    let mut sessions = Vec::with_capacity(CHANNELS);
    let mut info = None;
    for i in 0..CHANNELS {
        tracing::debug!(channel = i, "SFTP 채널 열기");
        match open_channel(&handle, t.ssh_shell).await {
            Ok((s, ext)) => {
                sessions.push(s);
                info.get_or_insert(ext);
            }
            Err(e) if i == 0 => return Err(e),
            // 서버 MaxSessions 등으로 일부만 열리면 열린 채널만 씁니다.
            Err(e) => {
                tracing::warn!(error = %e, opened = i, "SFTP 채널을 더 열지 못했습니다");
                break;
            }
        }
    }
    let ext = info.unwrap_or_default();
    Ok(Conn {
        handle,
        sessions,
        next: AtomicUsize::new(0),
        posix_rename: ext.posix_rename,
        statvfs: ext.statvfs,
        read_chunk: ext.read_len.map_or(READ_CHUNK, |l| l.min(READ_CHUNK)),
        write_chunk: ext
            .write_len
            .map_or(WRITE_CHUNK_MIN, |l| l.clamp(1, WRITE_CHUNK_MAX)),
    })
}

/// 키를 먼저, 실패하면 암호를 시도합니다.
async fn authenticate(handle: &mut client::Handle<HostKeyHandler>, t: &Target) -> Result<()> {
    let user = t.username.as_str();
    for key in &t.keys {
        let hash = if key.algorithm().is_rsa() {
            // 서버가 ext-info를 보내지 않으면 rsa-sha2-256을 씁니다.
            handle
                .best_supported_rsa_hash()
                .await
                .map_err(map_ssh)?
                .unwrap_or(Some(HashAlg::Sha256))
        } else {
            None
        };
        let res = handle
            .authenticate_publickey(user, PrivateKeyWithHashAlg::new(key.clone(), hash))
            .await
            .map_err(map_ssh)?;
        if res.success() {
            return Ok(());
        }
        tracing::debug!(user, "SFTP 공개 키 인증 실패");
    }
    if let Some(pw) = &t.password {
        let res = handle
            .authenticate_password(user, pw.expose_secret())
            .await
            .map_err(map_ssh)?;
        if res.success() {
            return Ok(());
        }
    }
    Err(Error::Auth(format!(
        "{}@{}:{} 로그인이 거부되었습니다",
        t.username, t.host, t.port
    )))
}

#[derive(Default)]
struct ServerExt {
    posix_rename: bool,
    statvfs: bool,
    read_len: Option<u64>,
    write_len: Option<u64>,
}

async fn open_channel(
    handle: &client::Handle<HostKeyHandler>,
    ssh_shell: bool,
) -> Result<(RawSftpSession, ServerExt)> {
    let mut ch = handle.channel_open_session().await.map_err(map_ssh)?;
    tracing::debug!(ssh_shell, "SSH 세션 열림, 파일 접근 요청");
    if ssh_shell {
        let script = include_str!("ssh_files.py");
        let command = format!("python3 -I -u -c '{}'", script.replace('\'', "'\\''"));
        ch.exec(true, command).await.map_err(map_ssh)?;
    } else {
        ch.request_subsystem(true, "sftp").await.map_err(map_ssh)?;
    }
    // request_subsystem은 요청 전송만 기다립니다. 거부 응답을 스트림으로 넘기면
    // SFTP INIT의 응답을 무한히 기다리므로 CHANNEL_SUCCESS를 먼저 확인합니다.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match ch.wait().await {
                Some(russh::ChannelMsg::Success) => return Ok(()),
                Some(russh::ChannelMsg::Failure) => return Err(Error::Config(if ssh_shell {
                    "서버가 SSH 명령 실행을 거부했습니다. 계정의 셸 실행 권한을 확인하세요".into()
                } else {
                    "서버가 SFTP 요청을 거부했습니다. SSH 셸 연결 방식을 선택하거나 SFTP 서비스를 활성화하고 계정 권한을 확인하세요".into()
                })),
                Some(russh::ChannelMsg::Close | russh::ChannelMsg::Eof) | None => {
                    return Err(Error::Config("파일 전송 시작 전에 서버가 채널을 닫았습니다".into()));
                }
                _ => {}
            }
        }
    }).await.map_err(|_| Error::Config("서버가 파일 전송 시작 요청에 응답하지 않습니다. 연결 방식과 서버 설정을 확인하세요".into()))??;
    tracing::debug!("SFTP 서브시스템 승인, 프로토콜 초기화");
    let cfg = russh_sftp::client::Config {
        request_timeout_secs: REQUEST_TIMEOUT_SECS,
        ..Default::default()
    };
    let mut raw = RawSftpSession::new_with_config(ch.into_stream(), cfg);
    let version = tokio::time::timeout(Duration::from_secs(10), raw.init())
        .await
        .map_err(|_| {
            Error::Config(if ssh_shell {
                "SSH 파일 접근 초기화 시간이 초과되었습니다. 서버의 Python 3 실행 환경을 확인하세요"
                    .into()
            } else {
                "SFTP 초기화 시간이 초과되었습니다".into()
            })
        })?
        .map_err(|e| {
            if ssh_shell {
                Error::Config(format!(
                    "SSH 파일 접근을 시작하지 못했습니다. 서버에 Python 3가 필요합니다: {e}"
                ))
            } else {
                map_sftp(e)
            }
        })?;
    tracing::debug!(extensions = ?version.extensions, "SFTP 프로토콜 초기화 완료");
    let has = |name: &str, ver: &str| version.extensions.get(name).is_some_and(|v| v == ver);
    let mut ext = ServerExt {
        posix_rename: has(POSIX_RENAME, "1"),
        statvfs: has(russh_sftp::extensions::STATVFS, "2"),
        ..Default::default()
    };
    if has(russh_sftp::extensions::LIMITS, "1") {
        let limits =
            russh_sftp::client::rawsession::Limits::from(raw.limits().await.map_err(map_sftp)?);
        ext.read_len = limits.read_len;
        ext.write_len = limits.write_len;
        raw.set_limits(limits);
    }
    Ok((raw, ext))
}

/// 상대 경로 루트(`~`, `data` 등)는 서버에서 절대 경로로 바꿉니다.
async fn resolve_root(s: &RawSftpSession, raw: &str) -> Result<String> {
    let root = if raw.starts_with('/') {
        normalize_root(raw)
    } else {
        let rel = raw.trim_start_matches("~/").trim_start_matches('~');
        let rel = if rel.is_empty() { "." } else { rel };
        let name = s.realpath(rel).await.map_err(|e| match map_sftp(e) {
            Error::NotFound => Error::Config(format!("원격 루트 {raw}가 없습니다")),
            other => other,
        })?;
        let abs = name
            .files
            .first()
            .map(|f| f.filename.clone())
            .ok_or_else(|| Error::Protocol("realpath 응답이 비어 있습니다".into()))?;
        normalize_root(&abs)
    };
    let attrs = s.stat(root.as_str()).await.map_err(|e| match map_sftp(e) {
        Error::NotFound => Error::Config(format!("원격 루트 {root}가 없습니다")),
        other => other,
    })?;
    if !attrs.attrs.is_dir() {
        return Err(Error::Config(format!(
            "원격 루트 {root}가 디렉터리가 아닙니다"
        )));
    }
    Ok(root)
}

// ---------- 경로와 오류 ----------

/// 후행 `/`와 중복 구분자를 정리합니다. 루트는 `/`입니다.
fn normalize_root(raw: &str) -> String {
    let mut out = String::new();
    for part in raw.split('/').filter(|p| !p.is_empty() && *p != ".") {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() { "/".into() } else { out }
}

/// `RemotePath`는 `..`를 거부하므로 결과는 항상 루트 하위입니다.
fn remote_path(root: &str, path: &RemotePath) -> String {
    if path.is_root() {
        root.to_owned()
    } else if root == "/" {
        path.as_str().to_owned()
    } else {
        format!("{root}{path}")
    }
}

/// 같은 디렉터리 안의 다른 이름
fn sibling(path: &str, name: &str) -> String {
    match path.rsplit_once('/') {
        Some(("", _)) | None => format!("/{name}"),
        Some((parent, _)) => format!("{parent}/{name}"),
    }
}

fn temp_name() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!(".~expandrv-{:x}-{nanos:x}-{seq:x}.tmp", std::process::id())
}

fn unix_secs(t: SystemTime) -> u32 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().min(u32::MAX as u64) as u32)
}

fn map_ssh(e: russh::Error) -> Error {
    match e {
        russh::Error::UnknownKey => Error::Auth("호스트 키를 신뢰할 수 없습니다".into()),
        russh::Error::ConnectionTimeout
        | russh::Error::KeepaliveTimeout
        | russh::Error::InactivityTimeout => Error::Timeout,
        other => Error::Network(other.to_string()),
    }
}

fn map_sftp(e: SftpError) -> Error {
    match e {
        SftpError::Status(s) => match s.status_code {
            StatusCode::Failure if s.error_message.starts_with("expandrv-errno:") => {
                match s.error_message.trim_start_matches("expandrv-errno:") {
                    "17" => Error::AlreadyExists,
                    "20" => Error::NotADirectory,
                    "21" => Error::IsADirectory,
                    "28" => Error::StorageFull,
                    "30" => Error::ReadOnly,
                    "39" => Error::DirectoryNotEmpty,
                    _ => Error::Protocol(s.error_message),
                }
            }
            StatusCode::NoSuchFile => Error::NotFound,
            StatusCode::PermissionDenied => Error::PermissionDenied,
            StatusCode::OpUnsupported => Error::Unsupported("SFTP 서버가 지원하지 않는 작업"),
            StatusCode::NoConnection | StatusCode::ConnectionLost => {
                Error::Network(s.status_code.to_string())
            }
            code => Error::Protocol(format!("{code}: {}", s.error_message)),
        },
        SftpError::Timeout => Error::Timeout,
        SftpError::IO(m) => Error::Network(m),
        // 채널이나 세션이 닫히면 응답 채널이 끊깁니다.
        SftpError::UnexpectedBehavior(m)
            if ["session closed", "sender dropped", "SendError", "RecvError"]
                .iter()
                .any(|k| m.contains(k)) =>
        {
            Error::Network(m)
        }
        other => Error::Protocol(other.to_string()),
    }
}

fn is_status(e: &SftpError, code: StatusCode) -> bool {
    matches!(e, SftpError::Status(s) if s.status_code == code)
}

fn is_failure(e: &SftpError) -> bool {
    is_status(e, StatusCode::Failure)
}

fn is_eof(e: &SftpError) -> bool {
    is_status(e, StatusCode::Eof)
}

// ---------- 작업 구현 ----------

fn entry_from(name: &str, a: &FileAttributes) -> Entry {
    let dir = a.is_dir();
    let size = if dir { 0 } else { a.size.unwrap_or(0) };
    Entry {
        name: name.to_owned(),
        kind: if dir { EntryKind::Dir } else { EntryKind::File },
        size,
        modified: a.mtime.map(|t| UNIX_EPOCH + Duration::from_secs(t.into())),
        created: None,
        etag: a.mtime.map(|t| format!("{t:x}-{size:x}")),
        readonly: a.permissions.is_some_and(|p| p & 0o222 == 0),
    }
}

/// 심볼릭 링크를 따라간 속성. 없으면 `None`입니다.
async fn stat_opt(s: &RawSftpSession, path: &str) -> Result<Option<FileAttributes>> {
    match s.stat(path).await {
        Ok(a) => Ok(Some(a.attrs)),
        Err(e) => match map_sftp(e) {
            Error::NotFound => Ok(None),
            other => Err(other),
        },
    }
}

async fn stat_entry(s: &RawSftpSession, path: &str, name: &str) -> Result<Entry> {
    let a = stat_opt(s, path).await?.ok_or(Error::NotFound)?;
    Ok(entry_from(name, &a))
}

async fn list_dir(s: &RawSftpSession, path: &str) -> Result<Vec<Entry>> {
    let handle = match s.opendir(path).await {
        Ok(h) => h.handle,
        // OpenSSH는 ENOTDIR을 NoSuchFile로 돌려주므로 실제 종류를 확인합니다.
        Err(e) if is_failure(&e) || is_status(&e, StatusCode::NoSuchFile) => {
            return Err(match stat_opt(s, path).await? {
                None => Error::NotFound,
                Some(a) if !a.is_dir() => Error::NotADirectory,
                Some(_) => map_sftp(e),
            });
        }
        Err(e) => return Err(map_sftp(e)),
    };
    let mut files = Vec::new();
    let res = loop {
        match s.readdir(handle.as_str()).await {
            Ok(name) => files.extend(name.files),
            Err(e) if is_eof(&e) => break Ok(()),
            Err(e) => break Err(map_sftp(e)),
        }
    };
    let _ = s.close(handle).await;
    res?;

    let mut out = Vec::with_capacity(files.len());
    for f in files {
        let name = f.filename;
        if name == "." || name == ".." {
            continue;
        }
        if RemotePath::root().join(&name).is_err() {
            tracing::warn!(dir = path, name = %name, "Windows에서 쓸 수 없는 이름을 건너뜁니다");
            continue;
        }
        let attrs = if f.attrs.is_symlink() {
            match stat_opt(s, &sibling_child(path, &name)).await {
                Ok(Some(a)) => a,
                // 끊어진 링크는 건너뜁니다.
                Ok(None) => continue,
                Err(e) => return Err(e),
            }
        } else {
            f.attrs
        };
        // 장치 파일과 FIFO 등 일반 파일이 아닌 항목은 숨깁니다.
        if attrs.permissions.is_some() && !attrs.is_dir() && !attrs.is_regular() {
            continue;
        }
        out.push(entry_from(&name, &attrs));
    }
    Ok(out)
}

fn sibling_child(dir: &str, name: &str) -> String {
    if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    }
}

async fn read_range(c: &Conn, path: &str, offset: u64, len: u64) -> Result<Bytes> {
    let s = c.pick();
    let handle = match s.open(path, OpenFlags::READ, FileAttributes::empty()).await {
        Ok(h) => h.handle,
        Err(e) => return Err(resolve_read_err(s, path, e).await),
    };
    let res = read_handle(s, &handle, offset, len, c.read_chunk).await;
    let _ = s.close(handle).await;
    match res {
        Ok(b) => Ok(b),
        Err(e) => Err(resolve_read_err(s, path, e).await),
    }
}

/// 디렉터리를 열거나 읽으면 서버가 일반 실패를 돌려주므로 종류를 확인합니다.
async fn resolve_read_err(s: &RawSftpSession, path: &str, e: SftpError) -> Error {
    if is_failure(&e)
        && let Ok(Some(a)) = stat_opt(s, path).await
        && a.is_dir()
    {
        return Error::IsADirectory;
    }
    map_sftp(e)
}

async fn read_handle(
    s: &RawSftpSession,
    handle: &str,
    offset: u64,
    len: u64,
    chunk: u64,
) -> Result<Bytes, SftpError> {
    let end = offset.saturating_add(len);
    let reqs = std::iter::successors(Some(offset), |o| o.checked_add(chunk))
        .take_while(|o| *o < end)
        .map(|o| (o, (end - o).min(chunk) as u32));
    let mut chunks = futures::stream::iter(reqs)
        .map(|(o, n)| async move { (n, read_exact_or_eof(s, handle, o, n).await) })
        .buffered(MAX_INFLIGHT);
    let mut out = Vec::with_capacity(len.min(16 << 20) as usize);
    while let Some((n, r)) = chunks.next().await {
        let data = r?;
        let short = data.len() < n as usize;
        out.extend_from_slice(&data);
        if short {
            break;
        }
    }
    Ok(Bytes::from(out))
}

/// 서버가 요청보다 짧게 줄 수 있으므로 EOF가 아니면 나머지를 이어 읽습니다.
async fn read_exact_or_eof(
    s: &RawSftpSession,
    handle: &str,
    offset: u64,
    len: u32,
) -> Result<Vec<u8>, SftpError> {
    let mut buf: Vec<u8> = Vec::new();
    while buf.len() < len as usize {
        let got = buf.len() as u32;
        match s.read(handle, offset + got as u64, len - got).await {
            Ok(d) if d.data.is_empty() => break,
            Ok(d) if buf.is_empty() => buf = d.data,
            Ok(d) => buf.extend_from_slice(&d.data),
            Err(e) if is_eof(&e) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(buf)
}

async fn write_file(c: &Conn, target: &str, name: &str, source: &WriteSource) -> Result<Entry> {
    let s = c.pick();
    let existing = stat_opt(s, target).await?;
    if existing.as_ref().is_some_and(|a| a.is_dir()) {
        return Err(Error::IsADirectory);
    }
    let tmp = sibling(target, &temp_name());
    let res = async {
        // 기존 파일의 권한을 이어받습니다.
        let attrs = FileAttributes {
            permissions: existing
                .as_ref()
                .and_then(|a| a.permissions)
                .map(|p| p & 0o7777),
            ..FileAttributes::empty()
        };
        let flags = OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE;
        let handle = s
            .open(tmp.as_str(), flags, attrs)
            .await
            .map_err(map_sftp)?
            .handle;
        let up = match source {
            WriteSource::Bytes(b) => upload(s, &handle, &b[..], c.write_chunk).await,
            WriteSource::File { path, .. } => match tokio::fs::File::open(path).await {
                Ok(f) => upload(s, &handle, f, c.write_chunk).await,
                Err(e) => Err(Error::Io(e)),
            },
        };
        let closed = s.close(handle).await.map_err(map_sftp);
        up?;
        closed?;
        replace_into(c, s, &tmp, target, existing.is_some()).await
    }
    .await;
    if let Err(e) = res {
        let _ = s.remove(tmp.as_str()).await;
        return Err(e);
    }
    stat_entry(s, target, name).await
}

/// 로컬 원본을 청크로 읽어 여러 쓰기 요청을 겹쳐 보냅니다. 메모리에는 진행 중인 청크만 둡니다.
async fn upload<R: AsyncRead + Unpin>(
    s: &RawSftpSession,
    handle: &str,
    mut src: R,
    chunk: u64,
) -> Result<()> {
    let chunk = chunk as usize;
    let mut inflight = FuturesUnordered::new();
    let (mut offset, mut eof) = (0u64, false);
    loop {
        while !eof && inflight.len() < MAX_INFLIGHT {
            let mut buf = vec![0u8; chunk];
            let n = fill(&mut src, &mut buf).await.map_err(Error::Io)?;
            eof = n < chunk;
            if n == 0 {
                break;
            }
            buf.truncate(n);
            inflight.push(s.write(handle, offset, buf));
            offset += n as u64;
        }
        match inflight.next().await {
            Some(r) => {
                r.map_err(map_sftp)?;
            }
            None => return Ok(()),
        }
    }
}

async fn fill<R: AsyncRead + Unpin>(src: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match src.read(&mut buf[n..]).await? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// `posix-rename` 확장이 있으면 원자적으로 덮어쓰고, 없으면 대상을 지운 뒤 이름을 바꿉니다.
async fn replace_into(
    c: &Conn,
    s: &RawSftpSession,
    from: &str,
    to: &str,
    target_exists: bool,
) -> Result<()> {
    if c.posix_rename {
        return posix_rename(s, from, to).await;
    }
    if target_exists {
        match s.remove(to).await.map_err(map_sftp) {
            Ok(_) | Err(Error::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    s.rename(from, to).await.map_err(map_sftp)?;
    Ok(())
}

fn posix_rename_payload(from: &str, to: &str) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + from.len() + to.len());
    for s in [from, to] {
        data.extend_from_slice(&(s.len() as u32).to_be_bytes());
        data.extend_from_slice(s.as_bytes());
    }
    data
}

async fn posix_rename(s: &RawSftpSession, from: &str, to: &str) -> Result<()> {
    match s
        .extended(POSIX_RENAME, posix_rename_payload(from, to))
        .await
        .map_err(map_sftp)?
    {
        Packet::Status(st) if st.status_code == StatusCode::Ok => Ok(()),
        Packet::Status(st) => Err(map_sftp(SftpError::Status(st))),
        _ => Err(Error::Protocol(
            "posix-rename 응답이 올바르지 않습니다".into(),
        )),
    }
}

async fn make_dir(s: &RawSftpSession, path: &str, name: &str) -> Result<Entry> {
    if let Err(e) = s.mkdir(path, FileAttributes::empty()).await {
        if is_failure(&e) && stat_opt(s, path).await?.is_some() {
            return Err(Error::AlreadyExists);
        }
        return Err(map_sftp(e));
    }
    stat_entry(s, path, name).await
}

async fn rename(c: &Conn, from: &str, to: &str, replace: bool) -> Result<()> {
    let s = c.pick();
    if from == to {
        return stat_opt(s, from).await?.map(|_| ()).ok_or(Error::NotFound);
    }
    match stat_opt(s, to).await? {
        Some(_) if !replace => Err(Error::AlreadyExists),
        Some(a) if a.is_dir() => Err(Error::IsADirectory),
        Some(_) => {
            // 대체 경로(삭제 후 이름 변경)에서 원본이 없으면 대상만 지워지므로 먼저 확인합니다.
            if !c.posix_rename && stat_opt(s, from).await?.is_none() {
                return Err(Error::NotFound);
            }
            replace_into(c, s, from, to, true).await
        }
        None => match s.rename(from, to).await {
            Ok(_) => Ok(()),
            Err(e) if is_failure(&e) && stat_opt(s, to).await?.is_some() => {
                Err(Error::AlreadyExists)
            }
            Err(e) => Err(map_sftp(e)),
        },
    }
}

async fn delete(s: &RawSftpSession, path: &str, kind: EntryKind) -> Result<()> {
    let res = match kind {
        EntryKind::File => s.remove(path).await,
        EntryKind::Dir => s.rmdir(path).await,
    };
    let Err(e) = res else {
        return Ok(());
    };
    if !is_failure(&e) {
        return Err(map_sftp(e));
    }
    // OpenSSH는 비어 있지 않은 디렉터리 등 여러 경우를 일반 실패로 돌려주므로 상태로 구분합니다.
    Err(match (kind, stat_opt(s, path).await?) {
        (_, None) => Error::NotFound,
        (EntryKind::File, Some(a)) if a.is_dir() => Error::IsADirectory,
        (EntryKind::Dir, Some(a)) if !a.is_dir() => Error::NotADirectory,
        (EntryKind::Dir, Some(_)) if !list_dir(s, path).await?.is_empty() => {
            Error::DirectoryNotEmpty
        }
        _ => map_sftp(e),
    })
}

/// SFTP v3는 접근 시각과 수정 시각을 함께 보내야 하므로 기존 접근 시각을 유지합니다.
async fn set_mtime(s: &RawSftpSession, path: &str, mtime: u32) -> Result<()> {
    let cur = stat_opt(s, path).await?.ok_or(Error::NotFound)?;
    let attrs = FileAttributes {
        atime: Some(cur.atime.unwrap_or(mtime)),
        mtime: Some(mtime),
        ..FileAttributes::empty()
    };
    s.setstat(path, attrs).await.map_err(map_sftp)?;
    Ok(())
}

async fn quota(c: &Conn, root: &str) -> Result<Quota> {
    if !c.statvfs {
        return Ok(Quota::default());
    }
    let v = c.pick().statvfs(root).await.map_err(map_sftp)?;
    let unit = if v.fragment_size > 0 {
        v.fragment_size
    } else {
        v.block_size
    };
    Ok(Quota {
        total: Some(v.blocks.saturating_mul(unit)),
        used: Some(v.blocks.saturating_sub(v.blocks_free).saturating_mul(unit)),
        free: Some(v.blocks_avail.saturating_mul(unit)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh_sftp::protocol::Status;

    #[tokio::test]
    async fn rejected_sftp_subsystem_returns_error_without_waiting_for_init() {
        use russh::{Channel, ChannelId, server};
        struct RejectSftp;
        impl server::Handler for RejectSftp {
            type Error = russh::Error;
            async fn auth_password(
                &mut self,
                _: &str,
                _: &str,
            ) -> std::result::Result<server::Auth, Self::Error> {
                Ok(server::Auth::Accept)
            }
            async fn channel_open_session(
                &mut self,
                _channel: Channel<server::Msg>,
                reply: server::ChannelOpenHandle,
                _: &mut server::Session,
            ) -> std::result::Result<(), Self::Error> {
                reply.accept().await;
                Ok(())
            }
            async fn subsystem_request(
                &mut self,
                channel: ChannelId,
                name: &str,
                session: &mut server::Session,
            ) -> std::result::Result<(), Self::Error> {
                assert_eq!(name, "sftp");
                session.channel_failure(channel)?;
                Ok(())
            }
        }
        let key = PrivateKey::new(
            russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[42; 32]).into(),
            "test",
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let known_hosts = dir.path().join("known_hosts");
        std::fs::write(
            &known_hosts,
            format!(
                "[127.0.0.1]:{port} {}\n",
                key.public_key().to_openssh().unwrap()
            ),
        )
        .unwrap();
        let config = Arc::new(server::Config {
            keys: vec![key],
            ..Default::default()
        });
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _ = server::run_stream(config, socket, RejectSftp)
                .await
                .unwrap()
                .await;
        });
        let target = Target {
            ssh_shell: false,
            host: "127.0.0.1".into(),
            port,
            username: "test".into(),
            known_hosts: vec![known_hosts],
            password: Some(SecretString::from("test")),
            keys: vec![],
            proxy: None,
        };
        let result = tokio::time::timeout(Duration::from_secs(3), open_conn(&target))
            .await
            .expect("SFTP 거부가 즉시 반환되어야 합니다");
        server.abort();
        assert!(
            matches!(result, Err(Error::Config(message)) if message.contains("서버가 SFTP 요청을 거부"))
        );
    }

    fn p(s: &str) -> RemotePath {
        RemotePath::parse(s).unwrap()
    }

    fn status(code: StatusCode) -> SftpError {
        SftpError::Status(Status {
            id: 1,
            status_code: code,
            error_message: "msg".into(),
            language_tag: "en".into(),
        })
    }

    #[test]
    fn maps_paths_under_root() {
        assert_eq!(remote_path("/", &p("/a/b")), "/a/b");
        assert_eq!(remote_path("/", &RemotePath::root()), "/");
        assert_eq!(remote_path("/srv/data", &p("/a/b")), "/srv/data/a/b");
        assert_eq!(remote_path("/srv/data", &RemotePath::root()), "/srv/data");
        assert_eq!(normalize_root("/srv//data/"), "/srv/data");
        assert_eq!(normalize_root("/./"), "/");
        assert_eq!(normalize_root(""), "/");
    }

    #[test]
    fn sibling_and_child_paths() {
        assert_eq!(sibling("/a/b.txt", "t"), "/a/t");
        assert_eq!(sibling("/b.txt", "t"), "/t");
        assert_eq!(sibling_child("/", "x"), "/x");
        assert_eq!(sibling_child("/a", "x"), "/a/x");
        let (a, b) = (temp_name(), temp_name());
        assert_ne!(a, b);
        assert!(a.starts_with(".~expandrv-") && a.ends_with(".tmp"));
    }

    #[test]
    fn maps_status_codes() {
        assert!(matches!(
            map_sftp(status(StatusCode::NoSuchFile)),
            Error::NotFound
        ));
        assert!(matches!(
            map_sftp(status(StatusCode::PermissionDenied)),
            Error::PermissionDenied
        ));
        assert!(matches!(
            map_sftp(status(StatusCode::ConnectionLost)),
            Error::Network(_)
        ));
        assert!(matches!(
            map_sftp(status(StatusCode::OpUnsupported)),
            Error::Unsupported(_)
        ));
        assert!(matches!(
            map_sftp(status(StatusCode::Failure)),
            Error::Protocol(_)
        ));
        assert!(matches!(map_sftp(SftpError::Timeout), Error::Timeout));
        assert!(matches!(
            map_sftp(SftpError::UnexpectedBehavior("session closed".into())),
            Error::Network(_)
        ));
        assert!(is_failure(&status(StatusCode::Failure)));
        assert!(is_eof(&status(StatusCode::Eof)));
    }

    #[test]
    fn posix_rename_payload_is_two_ssh_strings() {
        assert_eq!(
            posix_rename_payload("/a", "/bc"),
            [0, 0, 0, 2, b'/', b'a', 0, 0, 0, 3, b'/', b'b', b'c']
        );
    }

    #[test]
    fn entry_from_attrs() {
        let mut a = FileAttributes {
            size: Some(10),
            mtime: Some(1_700_000_000),
            permissions: Some(0o100444),
            ..FileAttributes::empty()
        };
        let e = entry_from("f", &a);
        assert_eq!((e.kind, e.size, e.readonly), (EntryKind::File, 10, true));
        assert_eq!(
            e.modified,
            Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        );
        a.permissions = Some(0o040755);
        let d = entry_from("d", &a);
        assert_eq!((d.kind, d.size, d.readonly), (EntryKind::Dir, 0, false));
        assert_eq!(unix_secs(UNIX_EPOCH - Duration::from_secs(5)), 0);
    }
}
