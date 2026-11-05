//! FTP/FTPS 백엔드. suppaftp(tokio, rustls)로 제어 연결 풀을 두고 수동 모드, 바이너리 전송으로 동작합니다.
//!
//! FTP 제어 연결은 한 번에 명령 하나만 처리하므로 로그인된 연결을 최대 [`POOL_SIZE`]개까지
//! 필요할 때 만들어 재사용합니다. 끊긴 연결은 버리고, 재사용한 연결이 끊겨 있었다면 새 연결로
//! 한 번 더 시도합니다.

mod listing;
mod wire;

use async_trait::async_trait;
use bytes::Bytes;
use expandrv_core::config::{FtpConfig, FtpTls, SecretResolver};
use expandrv_core::{
    Backend, Capabilities, Entry, EntryKind, Error, RemotePath, Result, WriteSource,
};
use secrecy::SecretString;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use suppaftp::tokio::{AsyncRustlsConnector, AsyncRustlsFtpStream};
use suppaftp::tokio_rustls::TlsConnector;
use suppaftp::tokio_rustls::rustls::{self, ClientConfig};
use suppaftp::types::{FileType, Mode};
use suppaftp::{FtpError, Status};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, Semaphore};
use wire::{Charset, Fail, PASS_PLACEHOLDER};

type FtpStream = AsyncRustlsFtpStream;
type OpFuture<'c, T> = Pin<Box<dyn Future<Output = std::result::Result<T, Fail>> + Send + 'c>>;

/// 동시에 유지할 로그인 연결 수 상한
const POOL_SIZE: usize = 4;
/// 연결, TLS 협상, 로그인까지의 제한 시간
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// 제어 명령만 쓰는 작업의 제한 시간
const OP_TIMEOUT: Duration = Duration::from_secs(120);
/// 전송 작업 제한 시간 계산에 쓰는 최저 전송 속도(바이트/초)
const MIN_RATE: u64 = 64 * 1024;
/// 이보다 오래 쉰 연결은 NOOP으로 살아 있는지 확인한 뒤 씁니다.
const IDLE_CHECK: Duration = Duration::from_secs(15);
const COPY_BUF: usize = 256 * 1024;

/// 서버 FEAT 응답에서 쓰는 기능
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Feats {
    mlst: bool,
    mfmt: bool,
    utf8: bool,
    size: bool,
    mdtm: bool,
    epsv: bool,
}

impl Feats {
    fn from_map(map: &suppaftp::types::Features) -> Self {
        let has = |k: &str| map.keys().any(|f| f.eq_ignore_ascii_case(k));
        Self {
            mlst: has("MLST") || has("MLSD"),
            mfmt: has("MFMT"),
            utf8: has("UTF8"),
            size: has("SIZE"),
            mdtm: has("MDTM"),
            epsv: has("EPSV"),
        }
    }
}

struct Params {
    host: String,
    port: u16,
    tls: FtpTls,
    tls_config: Arc<ClientConfig>,
    username: String,
    charset: Charset,
}

struct Conn {
    s: FtpStream,
    last_used: Instant,
    /// 작업은 성공했지만 제어 연결 상태를 장담할 수 없을 때 표시합니다.
    poisoned: bool,
    _live: LiveToken,
}

/// 열린 연결 수 집계. 연결이 버려지면 수를 줄이고 빈자리를 기다리는 작업을 깨웁니다.
struct LiveToken(Arc<PoolState>);

#[derive(Default)]
struct PoolState {
    live: AtomicUsize,
    freed: Notify,
}

impl LiveToken {
    fn new(state: &Arc<PoolState>) -> Self {
        state.live.fetch_add(1, Ordering::AcqRel);
        Self(Arc::clone(state))
    }
}

impl Drop for LiveToken {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::AcqRel);
        self.0.freed.notify_one();
    }
}

pub struct FtpBackend {
    params: Params,
    feats: Feats,
    /// 해석된 원격 루트 절대 경로. 루트 자체면 `/`이고 그 외에는 후행 `/`가 없습니다.
    root: String,
    idle: Mutex<Vec<Conn>>,
    permits: Semaphore,
    state: Arc<PoolState>,
}

impl FtpBackend {
    /// 연결과 로그인을 확인하고 백엔드를 만듭니다. 서버 인증서는 Windows 인증서 저장소로 검증합니다.
    pub async fn connect(cfg: &FtpConfig, secrets: &dyn SecretResolver) -> Result<Self> {
        let tls = default_tls_config()?;
        Self::connect_with_tls(cfg, secrets, tls).await
    }

    /// 인증서 고정 등 TLS 검증 방식을 직접 정해 연결합니다. 평문(`tls = "none"`)이면 무시합니다.
    pub async fn connect_with_tls(
        cfg: &FtpConfig,
        secrets: &dyn SecretResolver,
        tls_config: Arc<ClientConfig>,
    ) -> Result<Self> {
        let password = match &cfg.password {
            Some(src) => secrets.resolve(src)?,
            None => SecretString::from("anonymous@"),
        };
        let charset = Charset::new(&cfg.encoding)?.with_password(password);
        if cfg.tls == FtpTls::None {
            tracing::warn!(
                host = %cfg.host,
                "평문 FTP 연결입니다. 자격 증명과 파일 내용이 암호화되지 않습니다"
            );
        }
        let params = Params {
            host: cfg.host.clone(),
            port: cfg.port,
            tls: cfg.tls,
            tls_config,
            username: cfg.username.clone(),
            charset,
        };
        let (mut s, feats) = open(&params).await.map_err(Fail::into_error)?;
        let root = resolve_root(&mut s, &cfg.root)
            .await
            .map_err(wire::ftp_error)?;
        tracing::info!(
            host = %cfg.host,
            root = %root,
            mlst = feats.mlst,
            mfmt = feats.mfmt,
            "FTP 연결 확인"
        );
        let state = Arc::new(PoolState::default());
        Ok(Self {
            params,
            feats,
            root,
            idle: Mutex::new(vec![Conn {
                s,
                last_used: Instant::now(),
                poisoned: false,
                _live: LiveToken::new(&state),
            }]),
            permits: Semaphore::new(POOL_SIZE),
            state,
        })
    }

    fn remote(&self, path: &RemotePath) -> String {
        match (self.root.as_str(), path.is_root()) {
            (root, true) => root.to_owned(),
            ("/", false) => path.as_str().to_owned(),
            (root, false) => format!("{root}{path}"),
        }
    }

    fn take_idle(&self) -> Option<Conn> {
        self.idle.lock().unwrap_or_else(|e| e.into_inner()).pop()
    }

    fn put_idle(&self, mut c: Conn) {
        c.last_used = Instant::now();
        self.idle.lock().unwrap_or_else(|e| e.into_inner()).push(c);
        self.state.freed.notify_one();
    }

    /// 풀에서 연결을 꺼내거나 새로 만듭니다. 두 번째 값은 재사용 여부입니다.
    async fn checkout(&self) -> std::result::Result<(Conn, bool), Fail> {
        let deadline = Instant::now() + OP_TIMEOUT;
        loop {
            while let Some(mut c) = self.take_idle() {
                if c.last_used.elapsed() < IDLE_CHECK {
                    return Ok((c, true));
                }
                match tokio::time::timeout(Duration::from_secs(10), c.s.noop()).await {
                    Ok(Ok(())) => return Ok((c, true)),
                    _ => tracing::debug!("유휴 FTP 연결이 끊겨 버립니다"),
                }
            }
            match open(&self.params).await {
                Ok((s, _)) => {
                    let c = Conn {
                        s,
                        last_used: Instant::now(),
                        poisoned: false,
                        _live: LiveToken::new(&self.state),
                    };
                    return Ok((c, false));
                }
                // 서버의 동시 접속 제한에 걸리면 다른 작업이 연결을 돌려줄 때까지 기다립니다.
                Err(f)
                    if f.is_capacity_refusal() && self.state.live.load(Ordering::Acquire) > 0 =>
                {
                    tracing::debug!("FTP 서버 동시 접속 제한으로 유휴 연결을 기다립니다");
                    let wait = deadline.saturating_duration_since(Instant::now());
                    if tokio::time::timeout(wait, self.state.freed.notified())
                        .await
                        .is_err()
                    {
                        return Err(Fail::Timeout);
                    }
                }
                Err(f) => return Err(f),
            }
        }
    }

    /// 풀 연결 하나로 `op`를 실행합니다. 재사용한 연결이 끊겨 있었다면 새 연결로 한 번 더 시도합니다.
    async fn run<T, F>(&self, budget: Duration, mut op: F) -> Result<T>
    where
        T: Send,
        F: for<'c> FnMut(&'c mut Conn) -> OpFuture<'c, T> + Send,
    {
        let _permit = self.permits.acquire().await.map_err(|_| Error::Cancelled)?;
        let mut retried = false;
        loop {
            let (mut c, reused) = self.checkout().await.map_err(Fail::into_error)?;
            let res = match tokio::time::timeout(budget, op(&mut c)).await {
                Ok(r) => r,
                Err(_) => Err(Fail::Timeout),
            };
            match res {
                Ok(v) => {
                    if !c.poisoned {
                        self.put_idle(c);
                    }
                    return Ok(v);
                }
                Err(f) if f.is_broken() => {
                    drop(c);
                    if reused && !retried && f.is_retryable() {
                        tracing::debug!("재사용한 FTP 연결이 끊겨 새 연결로 다시 시도합니다");
                        retried = true;
                        continue;
                    }
                    return Err(f.into_error());
                }
                Err(f) => {
                    if !c.poisoned {
                        self.put_idle(c);
                    }
                    return Err(f.into_error());
                }
            }
        }
    }
}

fn transfer_budget(bytes: u64) -> Duration {
    OP_TIMEOUT + Duration::from_secs(bytes / MIN_RATE)
}

fn default_tls_config() -> Result<Arc<ClientConfig>> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .and_then(|b| b.with_platform_verifier())
        .map_err(|e| Error::Config(format!("TLS 설정 실패: {e}")))?
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// 연결, TLS, 로그인, FEAT, 문자셋, 전송 형식과 수동 모드까지 설정한 새 제어 연결
async fn open(p: &Params) -> std::result::Result<(FtpStream, Feats), Fail> {
    match tokio::time::timeout(CONNECT_TIMEOUT, open_inner(p)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(Fail::Ftp(e)),
        Err(_) => Err(Fail::Timeout),
    }
}

async fn open_inner(p: &Params) -> suppaftp::FtpResult<(FtpStream, Feats)> {
    let addr = (p.host.as_str(), p.port);
    // 데이터 연결은 같은 제어 연결의 TLS 세션을 재사용해야 하는 서버(vsftpd require_ssl_reuse 등)가 많고,
    // 서버 프로세스마다 티켓 키가 달라 다른 연결의 세션은 거부되므로 연결마다 세션 캐시를 따로 둡니다.
    let mut tls = (*p.tls_config).clone();
    tls.resumption = rustls::client::Resumption::default();
    let tls = Arc::new(tls);
    let connector = || AsyncRustlsConnector::from(TlsConnector::from(Arc::clone(&tls)));
    let mut s = match p.tls {
        FtpTls::Implicit => FtpStream::connect_secure_implicit(addr, connector(), &p.host).await?,
        FtpTls::None | FtpTls::Explicit => FtpStream::connect(addr).await?,
    };
    s.set_encoding(p.charset.clone()).await;
    if p.tls == FtpTls::Explicit {
        s = s.into_secure(connector(), &p.host).await?;
    }
    s.login(p.username.as_str(), PASS_PLACEHOLDER).await?;
    let feats = match s.feat().await {
        Ok(map) => Feats::from_map(&map),
        Err(FtpError::UnexpectedResponse(_) | FtpError::BadResponse) => Feats::default(),
        Err(e) => return Err(e),
    };
    if feats.utf8 && p.charset.is_utf8() {
        match s.opts("UTF8", Some("ON")).await {
            Ok(()) | Err(FtpError::UnexpectedResponse(_)) => {}
            Err(e) => return Err(e),
        }
    }
    s.transfer_type(FileType::Binary).await?;
    if feats.epsv {
        s.set_mode(Mode::ExtendedPassive);
    } else {
        // PASV가 사설 주소를 알려 주는 NAT 뒤 서버를 위해 제어 연결 주소를 씁니다.
        s.set_mode(Mode::Passive);
        s.set_passive_nat_workaround(true);
    }
    Ok((s, feats))
}

/// `/`로 시작하면 절대 경로, 아니면 로그인 직후 작업 디렉터리 기준 상대 경로로 해석합니다.
async fn resolve_root(s: &mut FtpStream, root: &str) -> suppaftp::FtpResult<String> {
    let root = root.trim();
    let base = if root.starts_with('/') {
        String::new()
    } else {
        s.pwd().await?
    };
    let mut out = String::new();
    for part in base
        .split('/')
        .chain(root.split('/'))
        .filter(|p| !p.is_empty() && *p != ".")
    {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('/');
    }
    Ok(out)
}

/// 원격 경로의 부모와 이름. 루트면 `None`입니다.
fn split_remote(remote: &str) -> Option<(String, &str)> {
    let idx = remote.rfind('/')?;
    let name = &remote[idx + 1..];
    if name.is_empty() {
        return None;
    }
    let parent = if idx == 0 { "/" } else { &remote[..idx] };
    Some((parent.to_owned(), name))
}

/// 응답 오류 없이 목록만 가져옵니다. MLSD를 우선하고, 없으면 CWD 후 LIST로 읽습니다.
async fn raw_list(
    c: &mut Conn,
    feats: Feats,
    remote: &str,
) -> std::result::Result<Vec<Entry>, Fail> {
    if feats.mlst {
        let lines = c.s.mlsd(Some(remote)).await?;
        return Ok(lines
            .iter()
            .filter_map(|l| listing::parse_mlsd_line(l))
            .collect());
    }
    // LIST 인자로 경로를 주면 일부 서버가 옵션으로 오해하거나 없는 경로에도 빈 목록을 돌려줍니다.
    c.s.cwd(remote).await?;
    let lines = c.s.list(None).await?;
    let now = chrono::Utc::now();
    Ok(lines
        .iter()
        .filter_map(|l| listing::parse_list_line(l, now))
        .collect())
}

async fn stat_on(c: &mut Conn, feats: Feats, remote: &str) -> std::result::Result<Entry, Fail> {
    let Some((parent, name)) = split_remote(remote) else {
        return Ok(Entry::dir(""));
    };
    if feats.mlst {
        match c.s.mlst(Some(remote)).await {
            Ok(line) => {
                return listing::parse_mlst_line(&line, name).ok_or_else(|| {
                    Fail::Fs(Error::Protocol(format!(
                        "MLST 응답을 해석할 수 없습니다: {line}"
                    )))
                });
            }
            Err(e) => {
                let f = Fail::from(e);
                if f.code() == Some(550) {
                    return Err(Fail::Fs(Error::NotFound));
                }
                if !matches!(f.code(), Some(500..=504)) {
                    return Err(f);
                }
            }
        }
    }
    let entries = match raw_list(c, feats, &parent).await {
        Ok(v) => v,
        Err(f) if f.code() == Some(550) => return Err(Fail::Fs(Error::NotFound)),
        Err(f) => return Err(f),
    };
    if let Some(e) = entries.into_iter().find(|e| e.name == name) {
        return Ok(e);
    }
    // 숨김 파일처럼 목록에 안 나오는 항목은 SIZE와 CWD로 확인합니다.
    if feats.size {
        match c.s.size(remote).await {
            Ok(size) => {
                let modified = if feats.mdtm {
                    c.s.mdtm(remote)
                        .await
                        .ok()
                        .and_then(listing::to_system_time)
                } else {
                    None
                };
                let mut e = Entry::file(name, size as u64);
                e.modified = modified;
                return Ok(e);
            }
            Err(FtpError::UnexpectedResponse(_)) => {}
            Err(e) => return Err(e.into()),
        }
    }
    match c.s.cwd(remote).await {
        Ok(()) => Ok(Entry::dir(name)),
        Err(FtpError::UnexpectedResponse(_)) => Err(Fail::Fs(Error::NotFound)),
        Err(e) => Err(e.into()),
    }
}

/// 디렉터리 작업의 응답 오류를 없음, 디렉터리 아님으로 구체화합니다.
async fn refine_dir_error(c: &mut Conn, feats: Feats, remote: &str, orig: Fail) -> Fail {
    if orig.code().is_none() {
        return orig;
    }
    match stat_on(c, feats, remote).await {
        Ok(e) if e.kind == EntryKind::File => Fail::Fs(Error::NotADirectory),
        Ok(_) => orig,
        Err(Fail::Fs(Error::NotFound)) => Fail::Fs(Error::NotFound),
        Err(f) if f.is_broken() => f,
        Err(_) => orig,
    }
}

async fn list_on(
    c: &mut Conn,
    feats: Feats,
    remote: &str,
) -> std::result::Result<Vec<Entry>, Fail> {
    match raw_list(c, feats, remote).await {
        Ok(v) => Ok(v),
        Err(f) => Err(refine_dir_error(c, feats, remote, f).await),
    }
}

/// 한 번 더 읽어 EOF인지 확인합니다. 데이터가 더 있으면 false입니다.
async fn at_eof<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> std::io::Result<bool> {
    let mut probe = [0u8; 1];
    Ok(r.read(&mut probe).await? == 0)
}

/// 전송을 중간에 끊은 뒤 남은 응답(426, 226, 225 등)을 소비하고 NOOP으로 동기화합니다.
async fn resync(s: &mut FtpStream) -> suppaftp::FtpResult<()> {
    for _ in 0..3 {
        match s.noop().await {
            Ok(()) => return Ok(()),
            Err(FtpError::UnexpectedResponse(r)) if r.status != Status::CommandOk => {
                // NOOP 응답 앞에 밀린 전송 응답이 있었습니다. 다음 줄을 NOOP 응답으로 확인합니다.
                match s.read_response(Status::CommandOk).await {
                    Ok(_) => return Ok(()),
                    Err(FtpError::UnexpectedResponse(_)) => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(FtpError::BadResponse)
}

async fn read_on(
    c: &mut Conn,
    feats: Feats,
    remote: &str,
    offset: u64,
    len: u64,
) -> std::result::Result<Bytes, Fail> {
    if offset > 0 {
        c.s.resume_transfer(offset as usize).await?;
    }
    let mut stream = match c.s.retr_as_stream(remote).await {
        Ok(s) => s,
        Err(e) => {
            let orig = Fail::from(e);
            if orig.code().is_none() {
                return Err(orig);
            }
            // REST가 EOF를 넘었거나 디렉터리를 RETR한 경우를 구분합니다.
            return match stat_on(c, feats, remote).await {
                Ok(e) if e.is_dir() => Err(Fail::Fs(Error::IsADirectory)),
                Ok(e) if offset >= e.size => Ok(Bytes::new()),
                Ok(_) => Err(orig),
                Err(f) => Err(f),
            };
        }
    };
    let mut buf = Vec::with_capacity(len.min(16 << 20) as usize);
    let read = async {
        (&mut stream).take(len).read_to_end(&mut buf).await?;
        if (buf.len() as u64) < len {
            return Ok::<_, std::io::Error>(true);
        }
        at_eof(&mut stream).await
    }
    .await;
    match read {
        Ok(true) => stream.finish().await?,
        Ok(false) => {
            // 남은 데이터를 버리고 데이터 연결을 닫습니다. 서버의 완료 응답은 resync가 소비합니다.
            drop(stream);
            if let Err(e) = resync(&mut c.s).await {
                tracing::debug!(error = %e, "부분 읽기 뒤 FTP 연결 동기화 실패, 연결을 버립니다");
                c.poisoned = true;
            }
        }
        Err(e) => {
            drop(stream);
            return Err(Fail::Ftp(FtpError::ConnectionError(e)));
        }
    }
    Ok(Bytes::from(buf))
}

async fn write_on(
    c: &mut Conn,
    remote: &str,
    source: &WriteSource,
) -> std::result::Result<(), Fail> {
    // 원본 파일을 먼저 열어 STOR 전에 로컬 오류를 드러냅니다.
    let mut file = match source {
        WriteSource::File { path, .. } => {
            Some(tokio::fs::File::open(path).await.map_err(Error::from)?)
        }
        WriteSource::Bytes(_) => None,
    };
    let mut stream = c.s.put_with_stream(remote).await?;
    let net = |e: std::io::Error| Fail::Ftp(FtpError::ConnectionError(e));
    match (source, file.as_mut()) {
        (WriteSource::Bytes(b), _) => stream.write_all(b).await.map_err(net)?,
        (WriteSource::File { .. }, Some(f)) => {
            let mut buf = vec![0u8; COPY_BUF];
            loop {
                let n = match f.read(&mut buf).await {
                    Ok(n) => n,
                    Err(e) => {
                        // 서버에는 잘린 파일이 남을 수 있으므로 연결도 정리합니다.
                        c.poisoned = true;
                        return Err(Fail::Fs(e.into()));
                    }
                };
                if n == 0 {
                    break;
                }
                stream.write_all(&buf[..n]).await.map_err(net)?;
            }
        }
        (WriteSource::File { .. }, None) => unreachable!(),
    }
    stream.finish().await?;
    Ok(())
}

async fn delete_on(
    c: &mut Conn,
    feats: Feats,
    remote: &str,
    kind: EntryKind,
) -> std::result::Result<(), Fail> {
    let res = match kind {
        EntryKind::File => c.s.rm(remote).await,
        EntryKind::Dir => c.s.rmdir(remote).await,
    };
    let Err(e) = res else {
        return Ok(());
    };
    let orig = Fail::from(e);
    if orig.code().is_none() {
        return Err(orig);
    }
    match kind {
        EntryKind::File => match stat_on(c, feats, remote).await {
            Ok(e) if e.is_dir() => Err(Fail::Fs(Error::IsADirectory)),
            Err(Fail::Fs(Error::NotFound)) => Err(Fail::Fs(Error::NotFound)),
            _ => Err(orig),
        },
        // RMD 실패 이유가 응답 문구로 구분되지 않는 서버가 많아 목록으로 확인합니다.
        EntryKind::Dir => match raw_list(c, feats, remote).await {
            Ok(v) if !v.is_empty() => Err(Fail::Fs(Error::DirectoryNotEmpty)),
            Ok(_) => Err(orig),
            Err(f) => Err(refine_dir_error(c, feats, remote, f).await),
        },
    }
}

async fn rename_on(
    c: &mut Conn,
    feats: Feats,
    from: &str,
    to: &str,
    replace: bool,
) -> std::result::Result<(), Fail> {
    let target = match stat_on(c, feats, to).await {
        Ok(e) => Some(e),
        Err(Fail::Fs(Error::NotFound)) => None,
        Err(f) => return Err(f),
    };
    if let Some(t) = &target {
        if !replace {
            return Err(Fail::Fs(Error::AlreadyExists));
        }
        if t.is_dir() {
            return Err(Fail::Fs(Error::IsADirectory));
        }
    }
    let Err(e) = c.s.rename(from, to).await else {
        return Ok(());
    };
    let f = Fail::from(e);
    if f.code().is_none() {
        return Err(f);
    }
    // 원본이 없어서 실패했다면 대상은 건드리지 않습니다.
    match stat_on(c, feats, from).await {
        Err(Fail::Fs(Error::NotFound)) => return Err(Fail::Fs(Error::NotFound)),
        Err(g) if g.is_broken() => return Err(g),
        _ => {}
    }
    if target.is_none() {
        return Err(f);
    }
    // 덮어쓰기를 거부하는 서버(IIS 등)는 대상을 지운 뒤 다시 시도합니다.
    c.s.rm(to).await?;
    c.s.rename(from, to).await?;
    Ok(())
}

#[async_trait]
impl Backend for FtpBackend {
    fn kind(&self) -> &'static str {
        "ftp"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            range_read: true,
            set_mtime: self.feats.mfmt,
            rename_dir: true,
            case_sensitive: true,
        }
    }

    async fn stat(&self, path: &RemotePath) -> Result<Entry> {
        if path.is_root() {
            return Ok(Entry::dir(""));
        }
        let (feats, remote) = (self.feats, self.remote(path));
        self.run(OP_TIMEOUT, move |c| {
            let remote = remote.clone();
            Box::pin(async move { stat_on(c, feats, &remote).await })
        })
        .await
    }

    async fn list(&self, path: &RemotePath) -> Result<Vec<Entry>> {
        let (feats, remote) = (self.feats, self.remote(path));
        self.run(OP_TIMEOUT, move |c| {
            let remote = remote.clone();
            Box::pin(async move { list_on(c, feats, &remote).await })
        })
        .await
    }

    async fn read_range(&self, path: &RemotePath, offset: u64, len: u64) -> Result<Bytes> {
        if len == 0 {
            return Ok(Bytes::new());
        }
        let (feats, remote) = (self.feats, self.remote(path));
        self.run(transfer_budget(len), move |c| {
            let remote = remote.clone();
            Box::pin(async move { read_on(c, feats, &remote, offset, len).await })
        })
        .await
    }

    async fn write(&self, path: &RemotePath, source: WriteSource) -> Result<Entry> {
        let (feats, remote) = (self.feats, self.remote(path));
        let name = path.name().unwrap_or("").to_owned();
        let size = source.len();
        let source = Arc::new(source);
        self.run(transfer_budget(size), move |c| {
            let (remote, source, name) = (remote.clone(), Arc::clone(&source), name.clone());
            Box::pin(async move {
                write_on(c, &remote, &source).await?;
                match stat_on(c, feats, &remote).await {
                    Ok(e) => Ok(e),
                    Err(f) if f.is_broken() => Err(f),
                    Err(_) => Ok(Entry::file(name, size)),
                }
            })
        })
        .await
    }

    async fn mkdir(&self, path: &RemotePath) -> Result<Entry> {
        let (feats, remote) = (self.feats, self.remote(path));
        let name = path.name().unwrap_or("").to_owned();
        self.run(OP_TIMEOUT, move |c| {
            let (remote, name) = (remote.clone(), name.clone());
            Box::pin(async move {
                if let Err(e) = c.s.mkdir(&remote).await {
                    let orig = Fail::from(e);
                    if orig.code().is_none() {
                        return Err(orig);
                    }
                    return match stat_on(c, feats, &remote).await {
                        Ok(_) => Err(Fail::Fs(Error::AlreadyExists)),
                        Err(f) if f.is_broken() => Err(f),
                        Err(_) => Err(orig),
                    };
                }
                match stat_on(c, feats, &remote).await {
                    Ok(e) => Ok(e),
                    Err(f) if f.is_broken() => Err(f),
                    Err(_) => Ok(Entry::dir(name)),
                }
            })
        })
        .await
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let (feats, src, dst) = (self.feats, self.remote(from), self.remote(to));
        self.run(OP_TIMEOUT, move |c| {
            let (src, dst) = (src.clone(), dst.clone());
            Box::pin(async move { rename_on(c, feats, &src, &dst, replace).await })
        })
        .await
    }

    async fn delete(&self, path: &RemotePath, kind: EntryKind) -> Result<()> {
        if path.is_root() {
            return Err(Error::PermissionDenied);
        }
        let (feats, remote) = (self.feats, self.remote(path));
        self.run(OP_TIMEOUT, move |c| {
            let remote = remote.clone();
            Box::pin(async move { delete_on(c, feats, &remote, kind).await })
        })
        .await
    }

    async fn set_mtime(&self, path: &RemotePath, mtime: SystemTime) -> Result<()> {
        if !self.feats.mfmt {
            return Err(Error::Unsupported("set_mtime"));
        }
        let remote = self.remote(path);
        let stamp = listing::format_mfmt_time(mtime);
        self.run(OP_TIMEOUT, move |c| {
            let cmd = format!("MFMT {stamp} {remote}");
            Box::pin(async move {
                c.s.custom_command(cmd, &[Status::File]).await?;
                Ok(())
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests;
