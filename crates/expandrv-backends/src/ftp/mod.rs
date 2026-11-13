//! FTP/FTPS 백엔드. suppaftp(tokio, rustls)로 제어 연결 풀을 두고 수동 모드, 바이너리 전송으로 동작합니다.
//!
//! FTP 제어 연결은 한 번에 명령 하나만 처리하므로 로그인된 연결을 최대 [`POOL_SIZE`]개까지
//! 필요할 때 만들어 재사용합니다. 끊긴 연결은 버리고, 재사용한 연결이 끊겨 있었다면 새 연결로
//! 한 번 더 시도합니다.

mod listing;
mod wire;

use crate::proxy::Proxy;
use async_trait::async_trait;
use bytes::Bytes;
use expandrv_core::config::{FtpConfig, FtpTls, SecretResolver};
use expandrv_core::{
    Backend, Capabilities, Entry, EntryKind, Error, RemotePath, Result, UploadHint, UploadSession,
    WriteSource,
};
use secrecy::SecretString;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use suppaftp::tokio::{AsyncRustlsConnector, AsyncRustlsFtpStream};
use suppaftp::tokio_rustls::TlsConnector;
use suppaftp::tokio_rustls::rustls::{self, ClientConfig};
use suppaftp::types::{FileType, Mode};
use suppaftp::{FtpError, Status};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use wire::{Charset, Fail, PASS_PLACEHOLDER};

type FtpStream = AsyncRustlsFtpStream;
type Transfer = suppaftp::tokio::TransferStream<suppaftp::tokio::AsyncRustlsStream>;
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
/// 내려받기 중 이 시간 동안 데이터가 한 바이트도 오지 않으면 연결을 버리고 다시 시도합니다.
/// 연결을 자주 끊는 서버에서 가끔 데이터 연결이 멈추므로 짧게 잡고 VFS 재시도에 맡깁니다.
const STALL_TIMEOUT: Duration = Duration::from_secs(3);
/// RETR 응답 대기 시간. NAS 디스크가 절전에서 깨어나는 시간을 고려합니다.
const RETR_TIMEOUT: Duration = Duration::from_secs(15);
/// 이 크기 이상의 읽기는 끝난 뒤 전송을 끊지 않고 다음 연속 읽기를 기다립니다.
const PARK_MIN_READ: u64 = 1 << 20;
/// 이어 받기를 기다리는 전송 수 상한과 유지 시간
const MAX_PARKED: usize = 2;
const PARK_TTL: Duration = Duration::from_secs(10);
/// 부분 읽기 뒤 제어 연결 동기화를 기다리는 시간
const RESYNC_TIMEOUT: Duration = Duration::from_secs(5);

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
    proxy: Option<Arc<Proxy>>,
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
    /// 예비 연결을 여는 중인지 여부. 한 번에 하나만 엽니다.
    refilling: AtomicBool,
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

/// 다음 연속 읽기를 기다리는 RETR 전송
struct Parked {
    id: u64,
    remote: String,
    /// 전송이 다음에 돌려줄 파일 위치
    next: u64,
    conn: Conn,
    open: Open,
}

/// 끊지 않고 남겨 둔 전송과 끝 확인에 미리 읽은 1바이트
struct Open {
    stream: Transfer,
    carry: u8,
}

pub struct FtpBackend {
    params: Arc<Params>,
    feats: Feats,
    /// 해석된 원격 루트 절대 경로. 루트 자체면 `/`이고 그 외에는 후행 `/`가 없습니다.
    root: String,
    idle: Arc<Mutex<Vec<Conn>>>,
    permits: Arc<Semaphore>,
    state: Arc<PoolState>,
    parked: Arc<Mutex<Vec<Parked>>>,
    park_seq: AtomicU64,
    clock: Arc<listing::ServerClock>,
}

impl FtpBackend {
    /// 연결과 로그인을 확인하고 백엔드를 만듭니다. 서버 인증서는 Windows 인증서 저장소로 검증합니다.
    pub async fn connect(cfg: &FtpConfig, secrets: &dyn SecretResolver) -> Result<Self> {
        Self::connect_via(cfg, secrets, None).await
    }

    /// 프록시를 거쳐 연결합니다. 데이터 연결도 같은 프록시로 FTP 호스트에 엽니다.
    pub async fn connect_via(
        cfg: &FtpConfig,
        secrets: &dyn SecretResolver,
        proxy: Option<Arc<Proxy>>,
    ) -> Result<Self> {
        Self::open_backend(cfg, secrets, configured_tls(cfg)?, proxy).await
    }

    /// 인증서 고정 등 TLS 검증 방식을 직접 정해 연결합니다. 평문(`tls = "none"`)이면 무시합니다.
    pub async fn connect_with_tls(
        cfg: &FtpConfig,
        secrets: &dyn SecretResolver,
        tls_config: Arc<ClientConfig>,
    ) -> Result<Self> {
        Self::open_backend(cfg, secrets, tls_config, None).await
    }

    async fn open_backend(
        cfg: &FtpConfig,
        secrets: &dyn SecretResolver,
        tls_config: Arc<ClientConfig>,
        proxy: Option<Arc<Proxy>>,
    ) -> Result<Self> {
        if proxy.is_some() && cfg.tls == FtpTls::Implicit {
            return Err(Error::Unsupported("프록시와 암묵적 TLS FTP 조합"));
        }
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
            proxy,
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
            params: Arc::new(params),
            feats,
            root,
            idle: Arc::new(Mutex::new(vec![Conn {
                s,
                last_used: Instant::now(),
                poisoned: false,
                _live: LiveToken::new(&state),
            }])),
            permits: Arc::new(Semaphore::new(POOL_SIZE)),
            state,
            parked: Arc::new(Mutex::new(Vec::new())),
            park_seq: AtomicU64::new(0),
            clock: Arc::new(listing::ServerClock::default()),
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

    fn put_idle(&self, c: Conn) {
        return_idle(&self.idle, &self.state, c);
    }

    /// 작업을 마친 연결을 풀로 돌려줍니다. 상태를 장담할 수 없으면 버리고 예비 연결을 엽니다.
    fn release(&self, c: Conn) {
        if c.poisoned {
            drop(c);
            self.refill();
        } else {
            self.put_idle(c);
        }
    }

    fn parked(&self) -> std::sync::MutexGuard<'_, Vec<Parked>> {
        self.parked.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `offset`에서 이어지는 대기 전송을 꺼냅니다.
    fn take_parked(&self, remote: &str, offset: u64) -> Option<Parked> {
        let mut parked = self.parked();
        let i = parked
            .iter()
            .position(|p| p.remote == remote && p.next == offset)?;
        Some(parked.remove(i))
    }

    /// 전송을 끊지 않고 남깁니다. 상한을 넘거나 [`PARK_TTL`] 안에 이어지지 않으면 정리합니다.
    fn park(&self, remote: String, next: u64, conn: Conn, open: Open) {
        let id = self.park_seq.fetch_add(1, Ordering::Relaxed);
        let evicted = {
            let mut parked = self.parked();
            parked.push(Parked {
                id,
                remote,
                next,
                conn,
                open,
            });
            let excess = parked.len().saturating_sub(MAX_PARKED);
            parked.drain(..excess).collect::<Vec<_>>()
        };
        drop(evicted);
        let list = Arc::clone(&self.parked);
        tokio::spawn(async move {
            tokio::time::sleep(PARK_TTL).await;
            let expired = {
                let mut parked = list.lock().unwrap_or_else(|e| e.into_inner());
                parked
                    .iter()
                    .position(|p| p.id == id)
                    .map(|i| parked.remove(i))
            };
            drop(expired);
        });
    }

    /// 파일이 바뀌면 이전 내용을 보내던 대기 전송을 버립니다.
    fn drop_parked(&self, remote: &str) {
        let stale: Vec<_> = {
            let mut parked = self.parked();
            let (stale, keep) = std::mem::take(&mut *parked)
                .into_iter()
                .partition(|p| p.remote == remote);
            *parked = keep;
            stale
        };
        drop(stale);
    }

    /// 가장 오래된 대기 전송을 버려 서버 연결 자리를 만듭니다.
    fn evict_parked(&self) -> bool {
        let oldest = {
            let mut parked = self.parked();
            (!parked.is_empty()).then(|| parked.remove(0))
        };
        oldest.is_some()
    }

    /// 대기 전송에서 이어 받습니다. 실패하면 연결을 버리고 오류를 돌려줍니다.
    async fn continue_parked(
        &self,
        p: Parked,
        len: u64,
        keep_open: bool,
    ) -> std::result::Result<Bytes, Fail> {
        let Parked {
            remote,
            next,
            mut conn,
            open,
            ..
        } = p;
        let Open { mut stream, carry } = open;
        let mut buf = Vec::with_capacity(len.min(16 << 20) as usize);
        buf.push(carry);
        let tail = read_until_stall(&mut stream, len, &mut buf).await;
        match settle(&mut conn, stream, tail, keep_open).await? {
            Some(open) => self.park(remote, next + buf.len() as u64, conn, open),
            None => self.release(conn),
        }
        Ok(Bytes::from(buf))
    }

    /// 새 RETR로 읽습니다. 많이 읽은 전송은 끊지 않고 다음 연속 읽기를 위해 남깁니다.
    async fn read_fresh(
        &self,
        remote: &str,
        offset: u64,
        len: u64,
        keep_open: bool,
    ) -> Result<Bytes> {
        let _permit = self.permits.acquire().await.map_err(|_| Error::Cancelled)?;
        let mut retried = false;
        loop {
            let (mut c, reused) = self.checkout().await.map_err(Fail::into_error)?;
            let res = tokio::time::timeout(
                transfer_budget(len),
                read_on(&mut c, self.feats, remote, offset, len, keep_open),
            )
            .await
            .unwrap_or(Err(Fail::Timeout));
            match res {
                Ok((b, Some(open))) => {
                    self.park(remote.to_owned(), offset + b.len() as u64, c, open);
                    return Ok(b);
                }
                Ok((b, None)) => {
                    self.release(c);
                    return Ok(b);
                }
                Err(f) if f.is_broken() => {
                    drop(c);
                    if reused && !retried && f.is_retryable() {
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

    /// 정상 작업 뒤 버린 연결 대신 예비 연결을 미리 엽니다.
    /// 부분 읽기마다 제어 연결을 닫는 서버에서 다음 요청이 접속과 로그인을 기다리지 않게 합니다.
    fn refill(&self) {
        let state = &self.state;
        if state.live.load(Ordering::Acquire) >= POOL_SIZE
            || !self
                .idle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
            || state.refilling.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let (params, idle, state) = (
            Arc::clone(&self.params),
            Arc::clone(&self.idle),
            Arc::clone(&self.state),
        );
        tokio::spawn(async move {
            match open(&params).await {
                Ok((s, _)) => {
                    let c = Conn {
                        s,
                        last_used: Instant::now(),
                        poisoned: false,
                        _live: LiveToken::new(&state),
                    };
                    idle.lock().unwrap_or_else(|e| e.into_inner()).push(c);
                    state.freed.notify_one();
                }
                Err(f) => tracing::debug!(error = %f.into_error(), "FTP 예비 연결 준비 실패"),
            }
            state.refilling.store(false, Ordering::Release);
        });
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
                Err(f) if f.is_capacity_refusal() && self.evict_parked() => {
                    tracing::debug!("FTP 서버 동시 접속 제한으로 이어 받기 대기 전송을 정리합니다");
                }
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
                    self.release(c);
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

/// 연결을 풀로 돌려줍니다. 세션처럼 백엔드 참조 없이 돌려줄 때도 씁니다.
fn return_idle(idle: &Mutex<Vec<Conn>>, state: &PoolState, mut c: Conn) {
    c.last_used = Instant::now();
    idle.lock().unwrap_or_else(|e| e.into_inner()).push(c);
    state.freed.notify_one();
}

// ---------- 스트리밍 업로드 ----------

/// STOR 데이터 연결을 열어 둔 세션. 순차 쓰기만 받고 확정 시 전송 완료 응답을 확인합니다.
struct FtpUpload {
    conn: Option<Conn>,
    stream: Option<Transfer>,
    remote: String,
    name: String,
    feats: Feats,
    clock: Arc<listing::ServerClock>,
    sent: u64,
    idle: Arc<Mutex<Vec<Conn>>>,
    state: Arc<PoolState>,
    _permit: OwnedSemaphorePermit,
}

impl FtpUpload {
    /// 데이터 연결을 끊고 서버에 남은 부분 파일을 지웁니다. 제어 연결을 되살리지 못하면 버립니다.
    async fn discard(&mut self) {
        drop(self.stream.take());
        let Some(mut c) = self.conn.take() else {
            return;
        };
        match tokio::time::timeout(RESYNC_TIMEOUT, resync(&mut c.s)).await {
            Ok(Ok(())) => {
                if let Err(e) = c.s.rm(&self.remote).await {
                    tracing::debug!(remote = %self.remote, error = %e, "취소된 업로드 부분 파일 정리 실패");
                }
                return_idle(&self.idle, &self.state, c);
            }
            _ => {
                tracing::debug!("취소된 업로드 뒤 FTP 제어 연결을 되살리지 못해 버립니다");
            }
        }
    }
}

#[async_trait]
impl UploadSession for FtpUpload {
    async fn write(&mut self, offset: u64, data: Bytes) -> Result<()> {
        if offset != self.sent {
            return Err(Error::Unsupported("FTP 순서 밖 쓰기"));
        }
        let stream = self.stream.as_mut().ok_or(Error::Cancelled)?;
        match tokio::time::timeout(OP_TIMEOUT, stream.write_all(&data)).await {
            Ok(Ok(())) => {
                self.sent += data.len() as u64;
                Ok(())
            }
            Ok(Err(e)) => Err(Error::Network(format!("FTP 데이터 전송 실패: {e}"))),
            Err(_) => Err(Error::Timeout),
        }
    }

    async fn flush(&mut self) -> Result<()> {
        let stream = self.stream.as_mut().ok_or(Error::Cancelled)?;
        tokio::time::timeout(OP_TIMEOUT, stream.flush())
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(|e| Error::Network(format!("FTP 데이터 전송 실패: {e}")))
    }

    async fn finish(mut self: Box<Self>) -> Result<Entry> {
        let stream = self.stream.take().ok_or(Error::Cancelled)?;
        let mut c = self.conn.take().ok_or(Error::Cancelled)?;
        let done = tokio::time::timeout(OP_TIMEOUT, stream.finish()).await;
        match done {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let f = Fail::Ftp(e);
                if !f.is_broken() {
                    return_idle(&self.idle, &self.state, c);
                }
                return Err(f.into_error());
            }
            Err(_) => return Err(Error::Timeout),
        }
        let entry = match stat_on(&mut c, self.feats, &self.remote).await {
            Ok(e) => Ok(e),
            Err(f) if f.is_broken() => Err(f.into_error()),
            Err(_) => Ok(Entry::file(self.name.clone(), self.sent)),
        };
        if !c.poisoned {
            return_idle(&self.idle, &self.state, c);
        }
        let mut entry = entry?;
        if self.feats.mlst {
            self.clock.observe_upload(&entry, SystemTime::now());
            self.clock.apply(&mut entry);
        }
        Ok(entry)
    }

    async fn abort(mut self: Box<Self>) {
        self.discard().await;
    }
}

fn transfer_budget(bytes: u64) -> Duration {
    OP_TIMEOUT + Duration::from_secs(bytes / MIN_RATE)
}

fn configured_tls(cfg: &FtpConfig) -> Result<Arc<ClientConfig>> {
    if let Some(pin) = &cfg.pinned_cert_sha256 {
        if cfg.tls == FtpTls::None {
            return Err(Error::Config(
                "인증서 지문을 사용하려면 TLS를 선택하세요".into(),
            ));
        }
        return Ok(Arc::new(crate::tls::pinned_client_config(
            crate::tls::parse_fingerprint(pin)?,
        )?));
    }
    default_tls_config()
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
    let mut s = match (&p.proxy, p.tls) {
        (Some(proxy), _) => {
            let stream = proxy
                .connect(&p.host, p.port)
                .await
                .map_err(FtpError::ConnectionError)?;
            // 프록시 뒤에서는 제어 연결의 상대 주소가 프록시이므로, 데이터 연결은 FTP 호스트와
            // 서버가 알려 준 포트로 프록시 터널을 엽니다.
            let (proxy, host) = (Arc::clone(proxy), p.host.clone());
            FtpStream::connect_with_stream(stream)
                .await?
                .passive_stream_builder(move |addr| {
                    let (proxy, host) = (Arc::clone(&proxy), host.clone());
                    Box::pin(async move {
                        proxy
                            .connect(&host, addr.port())
                            .await
                            .map_err(FtpError::ConnectionError)
                    })
                })
        }
        (None, FtpTls::Implicit) => {
            FtpStream::connect_secure_implicit(addr, connector(), &p.host).await?
        }
        (None, FtpTls::None | FtpTls::Explicit) => FtpStream::connect(addr).await?,
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

/// `offset`부터 `len` 바이트를 읽습니다. `keep_open`이면 데이터가 남은 전송을 끊지 않고 돌려줍니다.
async fn read_on(
    c: &mut Conn,
    feats: Feats,
    remote: &str,
    offset: u64,
    len: u64,
    keep_open: bool,
) -> std::result::Result<(Bytes, Option<Open>), Fail> {
    if offset > 0 {
        c.s.resume_transfer(offset as usize).await?;
    }
    // 데이터 연결 준비가 멈추면 연결을 버리고 VFS 재시도로 새 연결에서 다시 받습니다.
    let opened = tokio::time::timeout(RETR_TIMEOUT, c.s.retr_as_stream(remote))
        .await
        .map_err(|_| stalled(c, "RETR 응답", RETR_TIMEOUT))?;
    let mut stream = match opened {
        Ok(s) => s,
        Err(e) => {
            let orig = Fail::from(e);
            if orig.code().is_none() {
                return Err(orig);
            }
            // REST가 EOF를 넘었거나 디렉터리를 RETR한 경우를 구분합니다.
            let Ok(found) = tokio::time::timeout(RETR_TIMEOUT, stat_on(c, feats, remote)).await
            else {
                return Err(stalled(c, "RETR 실패 뒤 조회", RETR_TIMEOUT));
            };
            return match found {
                Ok(e) if e.is_dir() => Err(Fail::Fs(Error::IsADirectory)),
                Ok(e) if offset >= e.size => Ok((Bytes::new(), None)),
                Ok(_) => Err(orig),
                Err(f) => Err(f),
            };
        }
    };
    let mut buf = Vec::with_capacity(len.min(16 << 20) as usize);
    let tail = read_until_stall(&mut stream, len, &mut buf).await;
    let open = settle(c, stream, tail, keep_open).await?;
    Ok((Bytes::from(buf), open))
}

/// 읽기를 마친 뒤 전송 상태
#[derive(Debug, PartialEq, Eq)]
enum Tail {
    /// 서버가 전송을 끝냈습니다.
    Eof,
    /// 데이터가 더 있습니다. 끝 확인에 읽은 1바이트가 있으면 함께 둡니다.
    More(Option<u8>),
    /// 제한 시간 동안 데이터가 오지 않았습니다.
    Stalled,
}

/// 전송을 마무리하거나, 이어 받을 수 있으면 끊지 않고 돌려줍니다.
async fn settle(
    c: &mut Conn,
    stream: Transfer,
    tail: std::io::Result<Tail>,
    keep_open: bool,
) -> std::result::Result<Option<Open>, Fail> {
    match tail {
        Ok(Tail::Eof) => {
            // 데이터는 모두 받았으므로 완료 응답이 늦으면 연결만 버립니다.
            match tokio::time::timeout(STALL_TIMEOUT, stream.finish()).await {
                Ok(r) => r?,
                Err(_) => {
                    tracing::debug!("FTP 전송 완료 응답이 없어 연결을 버립니다");
                    c.poisoned = true;
                }
            }
            Ok(None)
        }
        Ok(Tail::More(Some(carry))) if keep_open => Ok(Some(Open { stream, carry })),
        Ok(Tail::More(_)) => {
            // 남은 데이터를 버리고 데이터 연결을 닫습니다. 서버의 완료 응답은 resync가 소비합니다.
            // 일부 서버(Synology 등)는 이때 제어 연결도 닫으므로 오래 기다리지 않습니다.
            drop(stream);
            match tokio::time::timeout(RESYNC_TIMEOUT, resync(&mut c.s)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::debug!(error = %e, "부분 읽기 뒤 FTP 연결 동기화 실패, 연결을 버립니다");
                    c.poisoned = true;
                }
                Err(_) => {
                    tracing::debug!("부분 읽기 뒤 FTP 응답이 없어 연결을 버립니다");
                    c.poisoned = true;
                }
            }
            Ok(None)
        }
        Ok(Tail::Stalled) => {
            drop(stream);
            Err(stalled(c, "데이터 전송", STALL_TIMEOUT))
        }
        Err(e) => {
            drop(stream);
            Err(Fail::Ftp(FtpError::ConnectionError(e)))
        }
    }
}

/// `buf`가 `len` 바이트가 될 때까지 읽습니다. [`STALL_TIMEOUT`] 동안 데이터가 오지 않으면 멈춤으로,
/// 다 읽은 뒤에는 1바이트를 더 읽어 전송이 끝났는지 확인합니다.
async fn read_until_stall<R: tokio::io::AsyncRead + Unpin>(
    r: &mut R,
    len: u64,
    buf: &mut Vec<u8>,
) -> std::io::Result<Tail> {
    let mut chunk = vec![0u8; (len.min(COPY_BUF as u64) as usize).max(1)];
    while (buf.len() as u64) < len {
        let want = chunk.len().min((len - buf.len() as u64) as usize);
        let Ok(n) = tokio::time::timeout(STALL_TIMEOUT, r.read(&mut chunk[..want])).await else {
            tracing::debug!(received = buf.len(), want = len, "FTP 데이터가 멈췄습니다");
            return Ok(Tail::Stalled);
        };
        match n? {
            0 => return Ok(Tail::Eof),
            n => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let mut probe = [0u8; 1];
    match tokio::time::timeout(STALL_TIMEOUT, r.read(&mut probe)).await {
        Ok(Ok(0)) => Ok(Tail::Eof),
        Ok(Ok(_)) => Ok(Tail::More(Some(probe[0]))),
        Ok(Err(e)) => Err(e),
        // 필요한 분량은 받았으므로 남은 데이터가 있다고 보고 연결을 정리합니다.
        Err(_) => Ok(Tail::More(None)),
    }
}

/// 응답 없는 연결을 버리고 VFS가 다시 시도할 수 있는 시간 초과로 알립니다.
fn stalled(c: &mut Conn, what: &str, after: Duration) -> Fail {
    tracing::warn!(
        "FTP {what} 단계가 {}초 동안 멈춰 연결을 버립니다",
        after.as_secs()
    );
    c.poisoned = true;
    Fail::Timeout
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
    fn metadata_epoch(&self) -> u64 {
        u64::from(self.clock.offset() != 0)
    }
    fn normalize_mtime(&self, mtime: SystemTime) -> SystemTime {
        listing::parse_mlsd_time(&listing::format_mfmt_time(mtime)).unwrap_or(mtime)
    }
    fn kind(&self) -> &'static str {
        "ftp"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            range_read: true,
            set_mtime: self.feats.mfmt,
            rename_dir: true,
            case_sensitive: true,
            sequential_transfer: true,
            streaming_upload: true,
            random_write: false,
        }
    }

    async fn stat(&self, path: &RemotePath) -> Result<Entry> {
        if path.is_root() {
            return Ok(Entry::dir(""));
        }
        let (feats, remote) = (self.feats, self.remote(path));
        let mut entry = self
            .run(OP_TIMEOUT, move |c| {
                let remote = remote.clone();
                Box::pin(async move { stat_on(c, feats, &remote).await })
            })
            .await?;
        if feats.mlst {
            self.clock.apply(&mut entry);
        }
        Ok(entry)
    }

    async fn list(&self, path: &RemotePath) -> Result<Vec<Entry>> {
        let (feats, remote) = (self.feats, self.remote(path));
        let mut entries = self
            .run(OP_TIMEOUT, move |c| {
                let remote = remote.clone();
                Box::pin(async move { list_on(c, feats, &remote).await })
            })
            .await?;
        if feats.mlst {
            for entry in &mut entries {
                self.clock.apply(entry);
            }
        }
        Ok(entries)
    }

    async fn read_range(&self, path: &RemotePath, offset: u64, len: u64) -> Result<Bytes> {
        if len == 0 {
            return Ok(Bytes::new());
        }
        let remote = self.remote(path);
        let keep_open = len >= PARK_MIN_READ;
        if let Some(p) = self.take_parked(&remote, offset) {
            let permit = self.permits.acquire().await.map_err(|_| Error::Cancelled)?;
            match self.continue_parked(p, len, keep_open).await {
                Ok(b) => return Ok(b),
                Err(f) => {
                    tracing::debug!(error = %f.into_error(), "FTP 이어 받기 실패, 새 전송으로 받습니다");
                }
            }
            drop(permit);
        }
        self.read_fresh(&remote, offset, len, keep_open).await
    }

    async fn write(&self, path: &RemotePath, source: WriteSource) -> Result<Entry> {
        let (feats, remote) = (self.feats, self.remote(path));
        self.drop_parked(&remote);
        let name = path.name().unwrap_or("").to_owned();
        let size = source.len();
        let source = Arc::new(source);
        let mut entry = self
            .run(transfer_budget(size), move |c| {
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
            .await?;
        if feats.mlst {
            self.clock.observe_upload(&entry, SystemTime::now());
            self.clock.apply(&mut entry);
        }
        Ok(entry)
    }

    async fn begin_upload(
        &self,
        path: &RemotePath,
        _hint: UploadHint,
    ) -> Result<Box<dyn UploadSession>> {
        let remote = self.remote(path);
        let name = path.name().ok_or(Error::IsADirectory)?.to_owned();
        self.drop_parked(&remote);
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Error::Cancelled)?;
        let mut retried = false;
        loop {
            let (mut c, reused) = self.checkout().await.map_err(Fail::into_error)?;
            match tokio::time::timeout(OP_TIMEOUT, c.s.put_with_stream(&remote)).await {
                Ok(Ok(stream)) => {
                    return Ok(Box::new(FtpUpload {
                        conn: Some(c),
                        stream: Some(stream),
                        remote,
                        name,
                        feats: self.feats,
                        clock: self.clock.clone(),
                        sent: 0,
                        idle: self.idle.clone(),
                        state: self.state.clone(),
                        _permit: permit,
                    }));
                }
                Ok(Err(e)) => {
                    let f = Fail::Ftp(e);
                    if f.is_broken() {
                        drop(c);
                        if reused && !retried && f.is_retryable() {
                            retried = true;
                            continue;
                        }
                    } else if !c.poisoned {
                        self.put_idle(c);
                    }
                    return Err(f.into_error());
                }
                Err(_) => return Err(Error::Timeout),
            }
        }
    }

    async fn mkdir(&self, path: &RemotePath) -> Result<Entry> {
        let (feats, remote) = (self.feats, self.remote(path));
        let name = path.name().unwrap_or("").to_owned();
        let mut entry = self
            .run(OP_TIMEOUT, move |c| {
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
            .await?;
        if feats.mlst {
            self.clock.apply(&mut entry);
        }
        Ok(entry)
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let (feats, src, dst) = (self.feats, self.remote(from), self.remote(to));
        self.drop_parked(&src);
        self.drop_parked(&dst);
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
        self.drop_parked(&remote);
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
