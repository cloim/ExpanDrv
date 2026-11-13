//! 파일 시스템 드라이버 콜백과 [`Backend`] 사이의 동기 VFS 계층.
//!
//! WinFsp 디스패처 스레드에서 호출하므로 공개 API는 동기 함수이고, 내부에서 tokio 런타임으로
//! 백엔드 비동기 호출을 실행합니다. tokio 작업 스레드 안에서 호출하면 안 됩니다.
//!
//! - 읽기: 고정 크기 블록 단위로 받아 메모리 LRU에 두고, 순차 읽기면 다음 블록을 미리 받습니다.
//! - 쓰기(실시간, 기본): 로컬 스테이징 파일에 기록하면서 백엔드 업로드 세션으로 바로 보냅니다(write-through).
//!   전송 창이 차면 쓰기 호출이 기다리므로 호출 프로그램의 진행률이 실제 전송을 따르고, 마지막 핸들 정리는
//!   원격 확정까지 기다립니다. 스트리밍할 수 없는 쓰기(기존 파일 일부 수정, 순서 밖 쓰기 등)는 정리 때
//!   전체 업로드를 동기로 수행합니다.
//! - 쓰기(백그라운드): 로컬 스테이징 파일에 기록하고 마지막 핸들 정리 후 백그라운드에서 업로드합니다(write-back).
//!   업로드 중 내용이 다시 바뀌면 진행 중 업로드를 취소하고 다음 정리 때 다시 올립니다.
//! - 메타: 메모리 TTL 캐시와 SQLite 저장소. 원격이 응답하지 않으면 저장된 목록으로 탐색을 이어갑니다.

mod block;
mod diskcache;
mod meta;
mod persist;
mod read_pattern;
mod scheduler;
mod throttle;

pub use diskcache::{DiskCacheOptions, generate_key as generate_cache_key};
pub use throttle::throttle;

use block::BlockCache;
use bytes::Bytes;
use diskcache::DiskCache;
pub use expandrv_core::config::WriteMode;
use expandrv_core::{
    Backend, Entry, EntryKind, Error, Quota, RemotePath, Result, UploadHint, UploadSession,
    WriteSource,
};
use meta::{Lookup, MetaCache};
use parking_lot::Mutex;
use persist::MetaStore;
use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::os::windows::fs::FileExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::{NamedTempFile, TempPath};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::AbortHandle;

/// 일시 오류로 실패한 원격 읽기를 다시 시도하는 횟수와 첫 대기 시간(이후 두 배씩)
const READ_RETRIES: u32 = 3;
const READ_RETRY_BASE: Duration = Duration::from_millis(200);
const OFFLINE_COOLDOWN: Duration = Duration::from_secs(30);
/// 이 시간 안에 데이터 전송(업로드, 블록 내려받기)이 성공했으면 짧은 확인 조회의 시간 초과를 단절로 보지 않습니다.
/// 메타데이터 응답은 근거로 삼지 않아, 응답을 멈춘 서버는 이전처럼 바로 오프라인 대기로 들어갑니다.
/// 큰 파일을 올리는 동안 서버가 목록 조회에 늦게 답하는 경우 드라이브 전체가 오프라인 대기로 바뀌지 않게 합니다.
const LIVENESS_WINDOW: Duration = Duration::from_secs(10);
const CACHE_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const QUOTA_TTL: Duration = Duration::from_secs(60);
/// 자동 재시도 횟수. 넘으면 다음 정리나 `sync` 때 다시 시도합니다.
const MAX_UPLOAD_RETRIES: u32 = 5;
/// 이 시간 안에 열거한 디렉터리만 원격 변경을 확인합니다.
const WATCH_WINDOW: Duration = Duration::from_secs(600);
const MAX_WATCHED_DIRS: usize = 64;
/// 소비자가 없을 때 쌓아 둘 변경 알림 상한
const MAX_PENDING_CHANGES: usize = 10_000;
/// 실시간 업로드에서 아직 전송되지 않은 채 쌓아 둘 바이트 상한. 차면 쓰기 호출이 기다립니다.
const STREAM_WINDOW: usize = 16 << 20;
/// 닫기 단계의 커밋이 실패한 뒤 백그라운드 재시도까지 기다리는 시간
const COMMIT_RETRY_DELAY: Duration = Duration::from_secs(2);
/// 동시에 열어 두는 실시간 업로드 세션 수. 세션은 연결 슬롯을 쥐므로 조회용 슬롯을 남깁니다.
const STREAM_SLOTS: usize = 2;
/// 이 시간 동안 쓰지 않은 읽기 블록은 메모리에서 비웁니다.
const BLOCK_IDLE: Duration = Duration::from_secs(20);
const BLOCK_TRIM_INTERVAL: Duration = Duration::from_secs(10);
/// 탐색기 미리보기와 속성 처리기처럼 짧게 여기저기 읽는 요청은 블록 전체 대신 이 크기만 받습니다.
const PROBE_CHUNK: u64 = 256 * 1024;
/// 이 크기 이하의 단일 읽기만 조각 읽기 대상입니다.
const PROBE_MAX_READ: usize = 128 * 1024;
/// 연속 읽기가 이만큼 쌓이면 일반 블록 읽기와 선행 읽기로 전환합니다.
const PROBE_ESCALATE: u64 = 1 << 20;
/// 파일 끝에서 이 거리 안의 조각 읽기는 파일 끝까지 받습니다.
const PROBE_TAIL: u64 = 1 << 20;
/// 조각 캐시 메모리 상한
const PROBE_CACHE_MAX: usize = 16 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
}

/// 폴링으로 감지한 원격 변경
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: RemotePath,
    pub kind: ChangeKind,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferEvent {
    Uploaded(RemotePath),
    Conflict(RemotePath),
    /// 호출 프로그램이 닫은 뒤의 커밋이 실패했습니다. 변경분은 저널에 남겨 백그라운드에서 다시 시도합니다.
    Failed(RemotePath, String),
}

#[derive(Debug, Clone)]
pub struct VfsOptions {
    pub read_only: bool,
    pub metadata_ttl: Duration,
    /// 쓰기 스테이징 파일 위치. 소유자만 접근 가능한 폴더여야 합니다.
    pub staging_dir: PathBuf,
    /// 백엔드 호출 한 번의 시간 상한(업로드 제외)
    pub op_timeout: Duration,
    pub block_size: u64,
    pub memory_cache_bytes: usize,
    /// 순차 읽기 때 미리 받을 블록 수
    pub readahead_blocks: u32,
    /// SQLite 메타 저장소 경로. `None`이면 오프라인 폴백을 쓰지 않습니다.
    pub meta_db: Option<PathBuf>,
    pub max_parallel_uploads: usize,
    /// 암호화 디스크 블록 캐시. `None`이면 메모리 캐시만 씁니다.
    pub disk_cache: Option<DiskCacheOptions>,
    /// 쓰기를 원격에 반영하는 시점
    pub write_mode: WriteMode,
}

impl VfsOptions {
    pub fn new(staging_dir: PathBuf) -> Self {
        Self {
            read_only: false,
            metadata_ttl: Duration::from_secs(30),
            staging_dir,
            op_timeout: Duration::from_secs(120),
            block_size: 4 << 20,
            memory_cache_bytes: 64 << 20,
            readahead_blocks: 4,
            meta_db: None,
            max_parallel_uploads: 4,
            disk_cache: None,
            write_mode: WriteMode::Through,
        }
    }
}

#[derive(Clone)]
pub struct Vfs {
    s: Arc<Shared>,
}

struct Shared {
    backend: Arc<dyn Backend>,
    background_backend: Arc<dyn Backend>,
    speculative_backend: Arc<dyn Backend>,
    prefetch_slots: Arc<Semaphore>,
    rt: tokio::runtime::Handle,
    opts: VfsOptions,
    meta: MetaCache,
    metadata_epoch: Mutex<u64>,
    store: Option<MetaStore>,
    blocks: BlockCache,
    /// 짧은 임의 읽기용 [`PROBE_CHUNK`] 조각 캐시(메모리 전용)
    probes: BlockCache,
    nodes: Mutex<HashMap<RemotePath, Arc<Node>>>,
    quota: Mutex<Option<(Quota, Instant)>>,
    uploads: Semaphore,
    /// 실시간 업로드 세션 슬롯. 비어 있으면 정리 때 전체 업로드로 전환합니다.
    stream_slots: Arc<Semaphore>,
    /// 최근 열거한 디렉터리와 그 시각
    watched: Mutex<HashMap<RemotePath, Instant>>,
    changes: Mutex<Vec<Change>>,
    /// `shutdown` 이후에는 새 업로드를 시작하지 않습니다(남은 분량은 저널로 넘김).
    closed: AtomicBool,
    disk: Option<DiskCache>,
    /// 원격에서 받고 있는 블록 수
    downloads: AtomicUsize,
    /// 연속 전송 백엔드에서 파일별로 하나씩 도는 순차 선행 읽기 범위
    seq_prefetch: Mutex<HashMap<RemotePath, SeqPrefetch>>,
    offline_until: Mutex<Option<Instant>>,
    /// 마지막으로 데이터 전송이 성공한 시각
    alive_at: Arc<Mutex<Option<Instant>>>,
    events: Mutex<Vec<TransferEvent>>,
    /// 마지막 업로드 집계. 노드 표 잠금이 바쁠 때 돌려줍니다.
    last_counts: Mutex<UploadCounts>,
    background: Mutex<Vec<AbortHandle>>,
}

/// 순차 선행 읽기가 받을 블록 범위. 작업이 도는 동안 읽기가 진행되면 `end`를 늘립니다.
#[derive(Clone, Copy)]
struct SeqPrefetch {
    /// 이 블록보다 앞은 건너뜁니다. 멀리 탐색하면 앞으로 옮깁니다.
    next: u64,
    end: u64,
}

struct ActiveDownload<'a>(&'a AtomicUsize);
impl Drop for ActiveDownload<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 열린 파일 또는 디렉터리 핸들. 같은 경로의 핸들은 하나의 노드를 공유합니다.
pub struct FileHandle {
    node: Arc<Node>,
    read_pattern: Mutex<read_pattern::ReadPattern>,
}

struct Node {
    kind: EntryKind,
    path: Mutex<RemotePath>,
    /// Cleanup 전인 핸들 수
    opens: AtomicUsize,
    state: Mutex<NodeState>,
    /// 마지막으로 확인한 업로드 상태([`UPLOAD_NONE`] 등). 상태 잠금이 바쁠 때 집계에 씁니다.
    upload_class: AtomicU8,
}

const UPLOAD_NONE: u8 = 0;
const UPLOAD_ACTIVE: u8 = 1;
const UPLOAD_WAITING: u8 = 2;
const UPLOAD_FAILED: u8 = 3;

struct NodeState {
    entry: Entry,
    staged: Option<NamedTempFile>,
    dirty: bool,
    /// 내용 변경 세대. 업로드 시작 세대와 다르면 업로드 결과를 버립니다.
    generation: u64,
    /// 진행 중 업로드와 그 업로드를 예약한 세대
    upload: Option<(u64, AbortHandle)>,
    failures: u32,
    /// 업로드 후 적용할 사용자 지정 수정 시각
    pending_mtime: Option<SystemTime>,
    deleted: bool,
    /// 업로드 저널에 기록한 스테이징 파일 경로
    journaled: Option<PathBuf>,
    /// 편집을 시작할 때의 원격 버전. 복구 업로드에서 충돌 판단에 씁니다.
    base_etag: Option<String>,
    /// 이전 실행의 저널에서 되살린 노드
    recovered: bool,
    /// 끊긴 스테이징 다운로드(파일, 받은 바이트, 원격 버전). 같은 버전이면 이어받습니다.
    partial: Option<(NamedTempFile, u64, String)>,
    /// 진행 중인 실시간 업로드 세션
    stream: Option<StreamState>,
    /// 스테이징 내용이 빈 파일에서 시작했는지 여부. 원격 내용을 내려받아 고치는 중이면 false입니다.
    fresh: bool,
    /// 스트리밍을 포기하고 정리 때 전체 업로드로 반영해야 하는 상태
    fallback: bool,
    /// 데이터를 쓰기 전에 호출 프로그램이 알려 준 최종 크기
    size_hint: Option<u64>,
    /// 정리 단계의 동기 커밋이 진행 중
    committing: bool,
    /// 원격에 아직 없는 새 파일(실시간 모드의 지연 생성). 확정 전까지 이름 변경과 삭제는 로컬에서만 처리합니다.
    local_only: bool,
    /// 백그라운드 업로드가 연결을 얻어 실제로 전송 중
    transferring: bool,
}

/// 업로드가 끝나지 않은 파일 수를 상태별로 나눈 값
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UploadCounts {
    /// 전송 중이거나, 호출 프로그램이 아직 쓰는 중이라 닫을 때 바로 반영할 파일
    pub uploading: usize,
    /// 백그라운드 업로드를 기다리는 파일
    pub waiting: usize,
    /// 마지막 업로드 시도가 실패해 다시 시도를 기다리는 파일
    pub failed: usize,
}

impl UploadCounts {
    pub fn total(&self) -> usize {
        self.uploading + self.waiting + self.failed
    }
}

/// 실시간 업로드 세션의 호출 측 상태. 세션 자체는 tokio 작업이 쥐고 명령 채널로 받습니다.
struct StreamState {
    _slot: OwnedSemaphorePermit,
    tx: mpsc::UnboundedSender<StreamCmd>,
    window: Arc<Semaphore>,
    failed: Arc<Mutex<Option<Error>>>,
    random: bool,
    /// 순차 전용 세션이 다음에 받아야 할 오프셋
    next: u64,
    /// 보낸 바이트 합계와 가장 큰 끝 오프셋. 둘이 같으면 겹침 없이 이어 보낸 것입니다.
    sent: u64,
    max_end: u64,
}

enum StreamCmd {
    Write(u64, Bytes, OwnedSemaphorePermit),
    Flush(oneshot::Sender<Result<()>>),
    Finish(oneshot::Sender<Result<Entry>>),
    Abort,
}

impl StreamState {
    fn abort(self) {
        let _ = self.tx.send(StreamCmd::Abort);
    }

    fn error(&self) -> Option<Error> {
        self.failed.lock().as_ref().map(clone_error)
    }
}

/// 세션을 소비하며 명령을 처리합니다. 한 번 실패하면 이후 명령은 같은 오류로 답합니다.
async fn pump_stream(
    mut session: Box<dyn UploadSession>,
    mut rx: mpsc::UnboundedReceiver<StreamCmd>,
    failed: Arc<Mutex<Option<Error>>>,
    alive: Arc<Mutex<Option<Instant>>>,
) {
    let mut error: Option<Error> = None;
    let record = |e: Error, error: &mut Option<Error>| {
        *failed.lock() = Some(clone_error(&e));
        *error = Some(e);
    };
    while let Some(cmd) = rx.recv().await {
        match cmd {
            StreamCmd::Write(offset, data, permit) => {
                if error.is_none() {
                    match session.write(offset, data).await {
                        Ok(()) => *alive.lock() = Some(Instant::now()),
                        Err(e) => record(e, &mut error),
                    }
                }
                drop(permit);
            }
            StreamCmd::Flush(reply) => {
                let r = match &error {
                    Some(e) => Err(clone_error(e)),
                    None => match session.flush().await {
                        Ok(()) => Ok(()),
                        Err(e) => {
                            let out = clone_error(&e);
                            record(e, &mut error);
                            Err(out)
                        }
                    },
                };
                let _ = reply.send(r);
            }
            StreamCmd::Finish(reply) => {
                let r = match &error {
                    Some(e) => {
                        session.abort().await;
                        Err(clone_error(e))
                    }
                    None => session.finish().await,
                };
                let _ = reply.send(r);
                return;
            }
            StreamCmd::Abort => {
                session.abort().await;
                return;
            }
        }
    }
    session.abort().await;
}

/// 잠금을 잡습니다. tokio 작업 스레드에서 잠금이 바쁘면 그 스레드를 런타임에 돌려준 뒤 기다립니다.
///
/// 파일 시스템 요청은 노드 잠금을 쥔 채 원격 응답을 기다릴 수 있습니다. 작업 스레드가 같은 잠금에서
/// 그냥 기다리면, 그 스레드가 맡던 네트워크 이벤트와 타이머 처리가 멈춰 원격 응답이 오지 않는 교착이
/// 생깁니다. 바쁘지 않으면 바로 잡으므로 평소 비용은 없습니다.
fn lock_in_task<T>(m: &Mutex<T>) -> parking_lot::MutexGuard<'_, T> {
    if let Some(guard) = m.try_lock() {
        return guard;
    }
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| m.lock())
        }
        _ => m.lock(),
    }
}

/// 오류를 여러 호출자에게 전달하기 위한 복제. 입출력 오류는 종류와 메시지를 보존합니다.
fn clone_error(e: &Error) -> Error {
    match e {
        Error::NotFound => Error::NotFound,
        Error::AlreadyExists => Error::AlreadyExists,
        Error::NotADirectory => Error::NotADirectory,
        Error::IsADirectory => Error::IsADirectory,
        Error::DirectoryNotEmpty => Error::DirectoryNotEmpty,
        Error::PermissionDenied => Error::PermissionDenied,
        Error::ReadOnly => Error::ReadOnly,
        Error::InvalidPath(s) => Error::InvalidPath(s.clone()),
        Error::Unsupported(s) => Error::Unsupported(s),
        Error::Auth(s) => Error::Auth(s.clone()),
        Error::Timeout => Error::Timeout,
        Error::Cancelled => Error::Cancelled,
        Error::StorageFull => Error::StorageFull,
        Error::Network(s) => Error::Network(s.clone()),
        Error::Protocol(s) => Error::Protocol(s.clone()),
        Error::Config(s) => Error::Config(s.clone()),
        Error::Io(e) => Error::Io(std::io::Error::new(e.kind(), e.to_string())),
    }
}

impl Drop for NodeState {
    /// 올리지 못한 변경분은 스테이징 파일을 지우지 않고 저널과 함께 다음 실행으로 넘깁니다.
    fn drop(&mut self) {
        if self.dirty
            && !self.deleted
            && self.journaled.is_some()
            && let Some(f) = self.staged.as_mut()
        {
            f.disable_cleanup(true);
        }
    }
}

impl NodeState {
    /// 내용이 바뀌기 직전에 호출합니다. 진행 중 업로드를 취소합니다.
    fn begin_change(&mut self) {
        if let Some((_, h)) = self.upload.take() {
            h.abort();
        }
        self.transferring = false;
        self.generation += 1;
    }

    /// 실시간 세션을 버리고 정리 때 전체 업로드로 전환합니다.
    fn drop_stream(&mut self) {
        if let Some(stream) = self.stream.take() {
            stream.abort();
        }
        self.fallback = true;
    }

    /// 스테이징을 빈 내용으로 되돌립니다. 이후 쓰기는 새 세션으로 스트리밍할 수 있습니다.
    fn reset_fresh(&mut self, staged: NamedTempFile) {
        if let Some(stream) = self.stream.take() {
            stream.abort();
        }
        self.staged = Some(staged);
        self.fresh = true;
        self.fallback = false;
        self.size_hint = None;
    }
}

impl FileHandle {
    pub fn path(&self) -> RemotePath {
        self.node.path.lock().clone()
    }

    pub fn is_dir(&self) -> bool {
        self.node.kind == EntryKind::Dir
    }
}

fn version(e: &Entry) -> String {
    match &e.etag {
        Some(t) => t.clone(),
        None => {
            let ns = e
                .modified
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            format!("{}-{ns}", e.size)
        }
    }
}

fn offline(e: &Error) -> bool {
    matches!(e, Error::Network(_) | Error::Timeout)
}

impl Shared {
    /// 서버 시각 보정처럼 해석 기준이 바뀌면 이전 목록을 원격 변경 기준으로 쓰지 않습니다.
    fn refresh_metadata_epoch(&self) {
        let mut previous = self.metadata_epoch.lock();
        let current = self.backend.metadata_epoch();
        if *previous != current {
            self.meta.clear();
            *previous = current;
        }
    }
    fn run<T>(&self, fut: impl Future<Output = Result<T>>) -> Result<T> {
        self.rt.block_on(async {
            tokio::time::timeout(self.opts.op_timeout, fut)
                .await
                .unwrap_or(Err(Error::Timeout))
        })
    }

    async fn remote_read<T>(
        &self,
        fut: impl Future<Output = Result<T>>,
        cached: bool,
    ) -> Result<T> {
        if self
            .offline_until
            .lock()
            .is_some_and(|until| Instant::now() < until)
        {
            return Err(Error::Network("원격 확인 대기 중".into()));
        }
        let timeout = if cached {
            self.opts.op_timeout.min(CACHE_PROBE_TIMEOUT)
        } else {
            self.opts.op_timeout
        };
        let result = tokio::time::timeout(timeout, fut)
            .await
            .unwrap_or(Err(Error::Timeout));
        // 저장본이 있는 짧은 확인 조회가 늦었을 뿐이고 서버가 최근까지 응답했다면, 이번 호출만
        // 저장본으로 답하고 드라이브 전체를 오프라인 대기로 돌리지 않습니다.
        let busy_but_alive =
            cached && matches!(result, Err(Error::Timeout)) && self.recently_alive();
        let mut until = self.offline_until.lock();
        if result.as_ref().is_err_and(offline) && !busy_but_alive {
            *until = Some(Instant::now() + OFFLINE_COOLDOWN);
        } else if result.is_ok() {
            *until = None;
        }
        result
    }

    fn mark_alive(&self) {
        *self.alive_at.lock() = Some(Instant::now());
    }

    fn recently_alive(&self) -> bool {
        self.alive_at
            .lock()
            .is_some_and(|at| at.elapsed() < LIVENESS_WINDOW)
    }

    fn spawn_background(&self, work: impl Future<Output = ()> + Send + 'static) {
        let mut tasks = self.background.lock();
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        tasks.retain(|task| !task.is_finished());
        tasks.push(self.rt.spawn(work).abort_handle());
    }

    fn event(&self, event: TransferEvent) {
        let mut events = self.events.lock();
        if events.len() == MAX_PENDING_CHANGES {
            events.remove(0);
        }
        events.push(event);
    }

    /// 메모리와 디스크 캐시에서 경로와 하위 경로의 블록을 지웁니다.
    fn drop_blocks(&self, path: &RemotePath) {
        self.blocks.invalidate(path);
        self.probes.invalidate(path);
        if let Some(d) = &self.disk {
            d.invalidate(path);
        }
    }

    /// 일시 오류(네트워크, 시간 초과, 서버 프로토콜 오류)면 지수 백오프로 다시 읽습니다.
    async fn read_retry(
        &self,
        path: &RemotePath,
        offset: u64,
        len: u64,
        speculative: bool,
    ) -> Result<Bytes> {
        let backend = if speculative {
            &self.speculative_backend
        } else {
            &self.backend
        };
        let mut delay = READ_RETRY_BASE;
        let mut attempt = 0;
        loop {
            match backend.read_range(path, offset, len).await {
                Err(e @ (Error::Network(_) | Error::Timeout | Error::Protocol(_)))
                    if attempt < READ_RETRIES =>
                {
                    attempt += 1;
                    tracing::debug!(%path, offset, attempt, error = %e, "읽기 재시도");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                r => return r,
            }
        }
    }

    /// 디스크 캐시, 원격 순서로 블록을 찾고 메모리 캐시에 넣습니다. 원격에서 받은 블록은 디스크에도 둡니다.
    async fn load_block(
        self: &Arc<Self>,
        path: &RemotePath,
        ver: &str,
        idx: u64,
        size: u64,
        speculative: bool,
    ) -> Result<Bytes> {
        if speculative
            && self
                .nodes
                .lock()
                .get(path)
                .is_none_or(|n| n.opens.load(Ordering::SeqCst) == 0)
        {
            return Err(Error::Cancelled);
        }
        if let Some(b) = self.disk.as_ref().and_then(|d| d.get(path, ver, idx)) {
            self.blocks.put(path, ver, idx, b.clone());
            return Ok(b);
        }
        let bs = self.blocks.block_size();
        let off = idx * bs;
        self.downloads.fetch_add(1, Ordering::SeqCst);
        let active = ActiveDownload(&self.downloads);
        let r = self
            .remote_read(
                self.read_retry(path, off, bs.min(size - off), speculative),
                false,
            )
            .await;
        drop(active);
        let b = r?;
        self.blocks.put(path, ver, idx, b.clone());
        if self.disk.is_some() {
            let (s, path, ver, data) = (self.clone(), path.clone(), ver.to_owned(), b.clone());
            let mut tasks = self.background.lock();
            if self.closed.load(Ordering::SeqCst) {
                return Ok(b);
            }
            tasks.retain(|task| !task.is_finished());
            let task = self.rt.spawn_blocking(move || {
                if let Some(d) = &s.disk
                    && let Err(e) = d.put(&path, &ver, idx, &data)
                {
                    tracing::debug!(%path, idx, error = %e, "디스크 캐시 저장 실패");
                }
            });
            tasks.push(task.abort_handle());
        }
        Ok(b)
    }

    /// 로컬에서 만들거나 바꾼 항목을 저장된 부모 목록에 반영합니다.
    /// 원격 확인 대기 중 오프라인 목록으로 폴백해도 새 항목을 찾을 수 있게 합니다.
    fn persist_entry(&self, path: &RemotePath, entry: &Entry) {
        if let Some(store) = &self.store
            && let Err(e) = store.save_entry(path, entry)
        {
            tracing::debug!(%path, error = %e, "항목 저장 실패");
        }
    }

    fn forget(&self, path: &RemotePath) {
        self.meta.invalidate_tree(path);
        self.drop_blocks(path);
        if let Some(store) = &self.store
            && let Err(e) = store.forget_tree(path)
        {
            tracing::warn!(%path, error = %e, "메타 저장소 정리 실패");
        }
    }

    /// 정리가 끝나고 업로드도 없는 노드를 핸들 테이블에서 뺍니다.
    fn release(&self, node: &Arc<Node>) {
        let mut nodes = lock_in_task(&self.nodes);
        if node.opens.load(Ordering::SeqCst) != 0 {
            return;
        }
        {
            let st = lock_in_task(&node.state);
            if (st.dirty && !st.deleted) || st.upload.is_some() || st.committing {
                return;
            }
        }
        let path = node.path.lock().clone();
        if nodes.get(&path).is_some_and(|n| Arc::ptr_eq(n, node)) {
            nodes.remove(&path);
        }
    }

    fn schedule_upload(self: &Arc<Self>, node: &Arc<Node>, delay: Duration) {
        let mut st = lock_in_task(&node.state);
        if !st.dirty || st.deleted || st.upload.is_some() || self.closed.load(Ordering::SeqCst) {
            return;
        }
        let generation = st.generation;
        let (s, n) = (self.clone(), node.clone());
        let task = self.rt.spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let Ok(_permit) = s.uploads.acquire().await else {
                return;
            };
            let job = {
                let mut st = lock_in_task(&n.state);
                let job = match &st.staged {
                    Some(staged) if st.generation == generation && !st.deleted => Some((
                        n.path.lock().clone(),
                        WriteSource::File {
                            path: staged.path().to_owned(),
                            size: st.entry.size,
                        },
                        st.pending_mtime,
                        st.recovered.then(|| st.base_etag.clone()).flatten(),
                    )),
                    _ => None,
                };
                st.transferring = job.is_some();
                job
            };
            let Some((path, source, mtime, recovered_base)) = job else {
                s.upload_done(&n, generation, None, None);
                return;
            };
            let mut conflict = None;
            let result = async {
                // 복구 업로드인데 그사이 원격이 바뀌었으면 덮어쓰지 않고 충돌 사본으로 올립니다.
                let mut target = path.clone();
                let mut remote_kept = None;
                if let Some(base) = &recovered_base
                    && let Ok(remote) = s.background_backend.stat(&path).await
                    && remote.etag.as_deref() != Some(base.as_str())
                {
                    target = conflict_path(&path, SystemTime::now())?;
                    remote_kept = Some(remote);
                }
                let mut e = s.background_backend.write(&target, source).await?;
                s.mark_alive();
                if let Some(m) = mtime.filter(|_| s.backend.capabilities().set_mtime) {
                    s.background_backend.set_mtime(&target, m).await?;
                    e.modified = Some(m);
                    e.etag = None;
                }
                if let Some(remote) = remote_kept {
                    tracing::warn!(%path, conflict = %target, "원격 파일이 바뀌어 충돌 사본으로 올렸습니다");
                    s.meta.invalidate(&target);
                    conflict = Some(target);
                    return Ok(remote);
                }
                Ok(e)
            }
            .await;
            s.upload_done(&n, generation, Some((path, result)), conflict);
        });
        st.upload = Some((generation, task.abort_handle()));
    }

    fn upload_done(
        self: &Arc<Self>,
        node: &Arc<Node>,
        generation: u64,
        outcome: Option<(RemotePath, Result<Entry>)>,
        conflict: Option<RemotePath>,
    ) {
        let mut st = lock_in_task(&node.state);
        // 취소된 이전 작업이 늦게 끝난 경우 현재 업로드 상태를 건드리지 않습니다.
        if !matches!(st.upload, Some((g, _)) if g == generation) {
            return;
        }
        st.upload = None;
        st.transferring = false;
        match outcome {
            Some((path, Ok(entry))) if st.generation == generation && !st.deleted => {
                tracing::debug!(%path, size = entry.size, "업로드 완료");
                self.event(match conflict {
                    Some(target) => TransferEvent::Conflict(target),
                    None => TransferEvent::Uploaded(path.clone()),
                });
                self.finalize_upload(&path, &mut st, entry);
            }
            Some((path, Err(e))) => {
                st.failures += 1;
                tracing::warn!(%path, error = %e, attempt = st.failures, "업로드 실패");
            }
            _ => {}
        }
        let retry = st.dirty && !st.deleted && st.failures <= MAX_UPLOAD_RETRIES;
        let delay = Duration::from_secs(1 << st.failures.min(6));
        drop(st);
        if retry && node.opens.load(Ordering::SeqCst) == 0 {
            let first = lock_in_task(&node.state).failures == 0;
            self.schedule_upload(node, if first { Duration::ZERO } else { delay });
        }
        self.release(node);
    }

    /// 원격에 반영된 항목으로 노드 상태, 저널, 메타와 블록 캐시를 맞춥니다.
    fn finalize_upload(&self, path: &RemotePath, st: &mut NodeState, entry: Entry) {
        self.refresh_metadata_epoch();
        st.entry = entry.clone();
        st.dirty = false;
        st.staged = None;
        st.pending_mtime = None;
        st.failures = 0;
        st.base_etag = None;
        st.recovered = false;
        st.fresh = false;
        st.fallback = false;
        st.size_hint = None;
        st.local_only = false;
        st.transferring = false;
        if let Some(stream) = st.stream.take() {
            stream.abort();
        }
        if st.journaled.take().is_some()
            && let Some(store) = &self.store
            && let Err(e) = store.journal_remove(path)
        {
            tracing::warn!(%path, error = %e, "업로드 저널 정리 실패");
        }
        self.persist_entry(path, &entry);
        self.meta.upsert_child(path, entry);
        self.drop_blocks(path);
    }

    /// 변경분을 업로드 저널에 기록합니다. 스테이징 파일이 바뀌었으면 기록을 갱신합니다.
    fn journal(&self, path: &RemotePath, st: &mut NodeState) {
        let (Some(store), Some(staged)) = (&self.store, &st.staged) else {
            return;
        };
        if st.journaled.as_deref() == Some(staged.path()) {
            return;
        }
        if st.journaled.is_none() {
            st.base_etag = st.entry.etag.clone();
        }
        match store.journal_put(path, staged.path(), st.base_etag.as_deref()) {
            Ok(()) => st.journaled = Some(staged.path().to_owned()),
            Err(e) => tracing::warn!(%path, error = %e, "업로드 저널 기록 실패"),
        }
    }

    /// 실시간 업로드 세션을 엽니다. 열지 못하면 정리 때 전체 업로드로 전환합니다.
    fn open_stream(self: &Arc<Self>, path: &RemotePath, st: &mut NodeState) {
        let caps = self.backend.capabilities();
        let hint = UploadHint {
            size: st.size_hint,
            new_file: st.local_only,
        };
        let Ok(slot) = self.stream_slots.clone().try_acquire_owned() else {
            tracing::debug!(%path, "실시간 업로드 슬롯이 없어 정리 때 올립니다");
            st.fallback = true;
            return;
        };
        let session = match self.run(self.backend.begin_upload(path, hint)) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(%path, error = %e, "실시간 업로드 세션을 열지 못해 정리 때 올립니다");
                st.fallback = true;
                return;
            }
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let failed = Arc::new(Mutex::new(None));
        self.spawn_background(pump_stream(
            session,
            rx,
            failed.clone(),
            self.alive_at.clone(),
        ));
        st.stream = Some(StreamState {
            _slot: slot,
            tx,
            window: Arc::new(Semaphore::new(STREAM_WINDOW)),
            failed,
            random: caps.random_write,
            next: 0,
            sent: 0,
            max_end: 0,
        });
    }

    /// 마지막 핸들이 닫힌 노드의 변경분을 지금 원격에 반영합니다(실시간 모드).
    ///
    /// 세션이 있으면 확정하고, 없거나 실패했으면 스테이징 파일 전체를 올립니다. 그래도 실패하면
    /// 저널에 남기고 [`TransferEvent::Failed`]로 알린 뒤 백그라운드 재시도를 예약합니다.
    /// `from_caller`가 true면 호출 프로그램이 결과를 받으므로 실패를 그대로 돌려주고 정리 때 다시
    /// 시도하게 둡니다. false면 알림과 저널, 백그라운드 재시도로 넘깁니다.
    fn commit_now(self: &Arc<Self>, node: &Arc<Node>, from_caller: bool) -> Result<()> {
        loop {
            let mut st = node.state.lock();
            if !st.dirty || st.deleted || st.committing || self.closed.load(Ordering::SeqCst) {
                return Ok(());
            }
            let Some(staged) = st.staged.as_ref() else {
                return Ok(());
            };
            let generation = st.generation;
            let path = node.path.lock().clone();
            let full = WriteSource::File {
                path: staged.path().to_owned(),
                size: st.entry.size,
            };
            let stream = st.stream.take();
            let mtime = st.pending_mtime;
            // 예약된 백그라운드 업로드가 있으면 지금 커밋으로 대신합니다.
            if let Some((_, h)) = st.upload.take() {
                h.abort();
            }
            st.committing = true;
            drop(st);

            let mut result = match stream {
                Some(stream) => {
                    let (reply, wait) = oneshot::channel();
                    match stream.tx.send(StreamCmd::Finish(reply)) {
                        Ok(()) => self.run(async { wait.await.unwrap_or(Err(Error::Cancelled)) }),
                        Err(_) => Err(Error::Cancelled),
                    }
                }
                None => Err(Error::Cancelled),
            };
            if let Err(e) = &result {
                if !matches!(e, Error::Cancelled) {
                    tracing::warn!(%path, error = %e, "실시간 업로드 확정 실패: 전체 업로드로 전환");
                }
                result = self.rt.block_on(self.backend.write(&path, full));
            }
            if let Ok(entry) = &mut result
                && let Some(m) = mtime.filter(|_| self.backend.capabilities().set_mtime)
            {
                match self.run(self.backend.set_mtime(&path, m)) {
                    Ok(()) => {
                        entry.modified = Some(m);
                        entry.etag = None;
                    }
                    Err(e) => tracing::warn!(%path, error = %e, "수정 시각 반영 실패"),
                }
            }

            let mut st = node.state.lock();
            st.committing = false;
            if st.generation != generation || st.deleted {
                // 커밋 중에 내용이 다시 바뀌었습니다. 핸들이 없으면 새 내용을 이어서 올립니다.
                drop(st);
                if node.opens.load(Ordering::SeqCst) == 0 {
                    continue;
                }
                return Ok(());
            }
            return match result {
                Ok(entry) => {
                    tracing::debug!(%path, size = entry.size, "실시간 업로드 완료");
                    self.event(TransferEvent::Uploaded(path.clone()));
                    self.finalize_upload(&path, &mut st, entry);
                    Ok(())
                }
                Err(e) if from_caller => {
                    tracing::warn!(%path, error = %e, "업로드 확정 실패: 호출 프로그램에 알립니다");
                    st.fallback = true;
                    Err(e)
                }
                Err(e) => {
                    tracing::warn!(%path, error = %e, "닫기 단계 업로드 실패: 저널에 남기고 다시 시도합니다");
                    self.journal(&path, &mut st);
                    st.failures = 1;
                    self.event(TransferEvent::Failed(path, e.to_string()));
                    drop(st);
                    if node.opens.load(Ordering::SeqCst) == 0 {
                        self.schedule_upload(node, COMMIT_RETRY_DELAY);
                    }
                    Err(e)
                }
            };
        }
    }

    fn fetch_block(
        self: &Arc<Self>,
        path: &RemotePath,
        ver: &str,
        idx: u64,
        size: u64,
    ) -> Result<Bytes> {
        if let Some(b) = self.blocks.get(path, ver, idx) {
            return Ok(b);
        }
        let (cell, _) = self.blocks.inflight(path, idx);
        let got = self.run(async {
            Ok(cell
                .get_or_init(|| async { self.load_block(path, ver, idx, size, false).await.ok() })
                .await
                .clone())
        });
        self.blocks.finish_inflight(path, idx);
        if got.as_ref().is_err_and(offline) {
            *self.offline_until.lock() = Some(Instant::now() + OFFLINE_COOLDOWN);
        } else if got.as_ref().is_ok_and(|b| b.is_some()) {
            self.mark_alive();
        }
        match got? {
            Some(b) => Ok(b),
            // 공유 셀에는 오류를 담지 않으므로 직접 다시 요청해 원래 오류를 돌려줍니다.
            None => self.run(self.load_block(path, ver, idx, size, false)),
        }
    }

    /// 메모리나 디스크에 이미 있는 블록만 돌려줍니다. 원격에는 요청하지 않습니다.
    fn cached_block(&self, path: &RemotePath, ver: &str, idx: u64) -> Option<Bytes> {
        if let Some(b) = self.blocks.get(path, ver, idx) {
            return Some(b);
        }
        let b = self.disk.as_ref()?.get(path, ver, idx)?;
        self.blocks.put(path, ver, idx, b.clone());
        Some(b)
    }

    /// `pos`를 포함한 데이터와 그 시작 위치를 돌려줍니다.
    /// 블록이 캐시에 없으면 블록 전체 대신 [`PROBE_CHUNK`] 조각만 받습니다.
    fn fetch_probe(
        self: &Arc<Self>,
        path: &RemotePath,
        ver: &str,
        pos: u64,
        size: u64,
    ) -> Result<(Bytes, u64)> {
        let bs = self.blocks.block_size();
        let idx = pos / bs;
        if let Some(b) = self.cached_block(path, ver, idx) {
            return Ok((b, idx * bs));
        }
        let chunk = pos / PROBE_CHUNK;
        let start = chunk * PROBE_CHUNK;
        if let Some(b) = self.probes.get(path, ver, chunk) {
            return Ok((b, start));
        }
        let load = || async {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            let _active = ActiveDownload(&self.downloads);
            // 파일 끝 근처는 끝까지 받습니다. 전송이 중간에 끊기지 않아 연결을 다시 쓸 수 있고,
            // 끝부분을 여러 번 읽는 PDF와 MP4 처리기도 한 번에 끝납니다.
            let len = match size - start {
                rest if rest <= PROBE_TAIL => rest,
                rest => PROBE_CHUNK.min(rest),
            };
            let all = self
                .remote_read(self.read_retry(path, start, len, false), false)
                .await?;
            let mut first = None;
            for (i, at) in (0..all.len()).step_by(PROBE_CHUNK as usize).enumerate() {
                let end = (at + PROBE_CHUNK as usize).min(all.len());
                let piece = all.slice(at..end);
                self.probes.put(path, ver, chunk + i as u64, piece.clone());
                first.get_or_insert(piece);
            }
            Ok::<_, Error>(first.unwrap_or_default())
        };
        // 탐색기와 백신이 같은 머리 부분을 동시에 읽어도 한 번만 받습니다.
        let (cell, _) = self.probes.inflight(path, chunk);
        let got = self.run(async {
            Ok(cell
                .get_or_init(|| async { load().await.ok() })
                .await
                .clone())
        });
        self.probes.finish_inflight(path, chunk);
        match got? {
            Some(b) => Ok((b, start)),
            None => Ok((self.run(load())?, start)),
        }
    }

    fn prefetch(self: &Arc<Self>, path: &RemotePath, ver: &str, from: u64, size: u64, window: u32) {
        if self.backend.capabilities().sequential_transfer {
            return self.prefetch_sequential(path, ver, from, size, window);
        }
        let bs = self.blocks.block_size();
        for idx in from..from + u64::from(window) {
            if idx * bs >= size || self.blocks.get(path, ver, idx).is_some() {
                continue;
            }
            let (cell, created) = self.blocks.inflight(path, idx);
            if !created {
                continue;
            }
            let Ok(permit) = self.prefetch_slots.clone().try_acquire_owned() else {
                self.blocks.finish_inflight(path, idx);
                break;
            };
            let (s, path, ver) = (self.clone(), path.clone(), ver.to_owned());
            self.spawn_background(async move {
                let _permit = permit;
                cell.get_or_init(|| async {
                    s.load_block(&path, &ver, idx, size, true).await.ok()
                })
                .await;
                s.blocks.finish_inflight(&path, idx);
            });
        }
    }

    /// 연속 전송 백엔드용 선행 읽기. 블록을 동시에 요청하면 전송마다 연결을 새로 열고 끊으므로
    /// 파일마다 작업 하나가 블록을 순서대로 받아 백엔드가 같은 전송을 이어 받게 합니다.
    fn prefetch_sequential(
        self: &Arc<Self>,
        path: &RemotePath,
        ver: &str,
        from: u64,
        size: u64,
        window: u32,
    ) {
        let bs = self.blocks.block_size();
        let end = (from + u64::from(window)).min(size.div_ceil(bs));
        if from >= end {
            return;
        }
        {
            let mut running = self.seq_prefetch.lock();
            if let Some(r) = running.get_mut(path) {
                // 이미 도는 작업이 범위를 넓혀 이어 받습니다. 멀리 건너뛰면 그 위치부터 받습니다.
                if from > r.end {
                    r.next = from;
                }
                r.end = r.end.max(end);
                return;
            }
            running.insert(path.clone(), SeqPrefetch { next: from, end });
        }
        let Ok(permit) = self.prefetch_slots.clone().try_acquire_owned() else {
            self.seq_prefetch.lock().remove(path);
            return;
        };
        let (s, path, ver) = (self.clone(), path.clone(), ver.to_owned());
        self.spawn_background(async move {
            let _permit = permit;
            let mut idx = from;
            loop {
                {
                    let mut running = s.seq_prefetch.lock();
                    let Some(r) = running.get(&path).copied() else {
                        break;
                    };
                    idx = idx.max(r.next);
                    if idx >= r.end {
                        running.remove(&path);
                        break;
                    }
                }
                if s.blocks.get(&path, &ver, idx).is_none() {
                    let (cell, created) = s.blocks.inflight(&path, idx);
                    if created {
                        let got = cell
                            .get_or_init(|| async {
                                s.load_block(&path, &ver, idx, size, true).await.ok()
                            })
                            .await
                            .is_some();
                        s.blocks.finish_inflight(&path, idx);
                        if !got {
                            // 파일을 닫았거나 연결 여유가 없으면 멈춥니다. 다음 읽기가 다시 시작합니다.
                            s.seq_prefetch.lock().remove(&path);
                            break;
                        }
                    }
                }
                idx += 1;
            }
        });
    }
}

/// 이전 목록과 새 목록의 차이
fn diff_listing(dir: &RemotePath, old: &[Entry], new: &[Entry]) -> Vec<Change> {
    let before: HashMap<&str, &Entry> = old.iter().map(|e| (e.name.as_str(), e)).collect();
    let after: HashMap<&str, &Entry> = new.iter().map(|e| (e.name.as_str(), e)).collect();
    let mut out = Vec::new();
    let mut push = |name: &str, kind, is_dir| {
        if let Ok(path) = dir.join(name) {
            out.push(Change { path, kind, is_dir });
        }
    };
    for (name, e) in &after {
        match before.get(name) {
            None => push(name, ChangeKind::Added, e.is_dir()),
            Some(o) if o.kind != e.kind => {
                push(name, ChangeKind::Removed, o.is_dir());
                push(name, ChangeKind::Added, e.is_dir());
            }
            // 시각을 설정한 직후처럼 모르는 버전 값은 다음 목록으로 확인합니다.
            Some(o)
                if !e.is_dir()
                    && (o.size != e.size
                        || o.modified.zip(e.modified).is_some_and(|(a, b)| a != b)
                        || o.etag
                            .as_ref()
                            .zip(e.etag.as_ref())
                            .is_some_and(|(a, b)| a != b)) =>
            {
                push(name, ChangeKind::Modified, false)
            }
            Some(_) => {}
        }
    }
    for (name, o) in &before {
        if !after.contains_key(name) {
            push(name, ChangeKind::Removed, o.is_dir());
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

impl Shared {
    /// 이전 실행의 업로드 저널을 읽어 남은 변경분을 다시 올립니다. 되살린 개수를 돌려줍니다.
    fn recover(self: &Arc<Self>) -> usize {
        let Some(store) = &self.store else {
            return 0;
        };
        let records = match store.journal_all() {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "업로드 저널을 읽지 못했습니다");
                return 0;
            }
        };
        let mut count = 0;
        for (path, staging, base_etag) in records {
            let file = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&staging)
            {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!(%path, staging = %staging.display(), error = %e, "스테이징 파일이 없어 업로드를 복구하지 못했습니다");
                    let _ = store.journal_remove(&path);
                    continue;
                }
            };
            let temp = match TempPath::try_from_path(&staging) {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(%path, error = %e, "스테이징 경로를 열지 못했습니다");
                    continue;
                }
            };
            let meta = file.metadata().ok();
            let mut entry = Entry::file(
                path.name().unwrap_or_default(),
                meta.as_ref().map_or(0, |m| m.len()),
            );
            entry.modified = meta.and_then(|m| m.modified().ok());
            let node = Arc::new(Node {
                kind: EntryKind::File,
                path: Mutex::new(path.clone()),
                opens: AtomicUsize::new(0),
                upload_class: AtomicU8::new(UPLOAD_NONE),
                state: Mutex::new(NodeState {
                    entry,
                    staged: Some(NamedTempFile::from_parts(file, temp)),
                    dirty: true,
                    generation: 0,
                    upload: None,
                    failures: 0,
                    pending_mtime: None,
                    deleted: false,
                    journaled: Some(staging),
                    base_etag,
                    recovered: true,
                    partial: None,
                    stream: None,
                    fresh: false,
                    fallback: false,
                    size_hint: None,
                    committing: false,
                    local_only: false,
                    transferring: false,
                }),
            });
            self.nodes.lock().insert(path, node.clone());
            self.schedule_upload(&node, Duration::ZERO);
            count += 1;
        }
        count
    }

    /// 최근 열거한 디렉터리를 다시 읽어 마지막 목록과 비교합니다.
    async fn poll_changes(&self) -> Vec<Change> {
        self.refresh_metadata_epoch();
        let dirs: Vec<RemotePath> = {
            let mut watched = self.watched.lock();
            watched.retain(|_, at| at.elapsed() < WATCH_WINDOW);
            let mut v: Vec<_> = watched.iter().map(|(p, at)| (p.clone(), *at)).collect();
            v.sort_by_key(|e| std::cmp::Reverse(e.1));
            v.into_iter()
                .take(MAX_WATCHED_DIRS)
                .map(|(p, _)| p)
                .collect()
        };
        let mut out = Vec::new();
        for dir in dirs {
            let Some(old) = self.meta.snapshot(&dir) else {
                continue;
            };
            let mut new = match self
                .remote_read(self.background_backend.list(&dir), true)
                .await
            {
                Ok(l) => l,
                Err(Error::NotFound | Error::NotADirectory) => {
                    self.watched.lock().remove(&dir);
                    continue;
                }
                Err(e) => {
                    tracing::debug!(%dir, error = %e, "변경 확인 실패");
                    continue;
                }
            };
            self.refresh_metadata_epoch();
            // 작업 중인 로컬 항목은 비교 기준을 유지하고 원격의 중간 상태를 가져오지 않습니다.
            // 노드 표와 상태 모두 기다리지 않으며 확정과의 경합은 replace_dir_if로 확인합니다.
            let ignored: Vec<String> = {
                let Some(nodes) = self.nodes.try_lock() else {
                    continue;
                };
                nodes
                    .iter()
                    .filter(|(path, _)| path.parent().as_ref() == Some(&dir))
                    .filter(|(_, node)| {
                        node.state
                            .try_lock()
                            .is_none_or(|st| st.dirty || st.local_only)
                    })
                    .filter_map(|(path, _)| path.name().map(str::to_owned))
                    .collect()
            };
            new.retain(|e| !ignored.contains(&e.name));
            new.extend(old.iter().filter(|e| ignored.contains(&e.name)).cloned());
            let diff = diff_listing(&dir, &old, &new);
            if !self.meta.replace_dir_if(&dir, &old, Arc::new(new.clone())) || diff.is_empty() {
                continue;
            }
            if let Some(store) = &self.store
                && let Err(e) = store.save_dir(&dir, &new)
            {
                tracing::warn!(%dir, error = %e, "메타 저장 실패");
            }
            for c in diff.iter().filter(|c| c.kind != ChangeKind::Added) {
                self.drop_blocks(&c.path);
                if c.is_dir {
                    self.meta.invalidate_tree(&c.path);
                }
            }
            for c in &diff {
                let before = old.iter().find(|e| Some(e.name.as_str()) == c.path.name());
                let after = new.iter().find(|e| Some(e.name.as_str()) == c.path.name());
                tracing::debug!(path = %c.path, kind = ?c.kind, ?before, ?after, "원격 변경 비교");
            }
            tracing::debug!(%dir, count = diff.len(), "원격 변경 감지");
            out.extend(diff);
        }
        out
    }
}

impl Vfs {
    pub fn new(
        backend: Arc<dyn Backend>,
        rt: tokio::runtime::Handle,
        opts: VfsOptions,
    ) -> Result<Self> {
        std::fs::create_dir_all(&opts.staging_dir)?;
        let store = opts.meta_db.as_deref().map(MetaStore::open).transpose()?;
        let disk = opts.disk_cache.as_ref().map(DiskCache::open).transpose()?;
        let [backend, background_backend, speculative_backend] = scheduler::schedule(backend);
        let metadata_epoch = backend.metadata_epoch();
        let s = Arc::new(Shared {
            backend,
            background_backend,
            speculative_backend,
            prefetch_slots: Arc::new(Semaphore::new(2)),
            rt,
            meta: MetaCache::new(opts.metadata_ttl),
            metadata_epoch: Mutex::new(metadata_epoch),
            store,
            blocks: BlockCache::new(opts.block_size, opts.memory_cache_bytes),
            probes: BlockCache::new(
                PROBE_CHUNK,
                (opts.memory_cache_bytes / 4).min(PROBE_CACHE_MAX),
            ),
            nodes: Mutex::new(HashMap::new()),
            quota: Mutex::new(None),
            uploads: Semaphore::new(opts.max_parallel_uploads.max(1)),
            stream_slots: Arc::new(Semaphore::new(STREAM_SLOTS)),
            watched: Mutex::new(HashMap::new()),
            changes: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            disk,
            downloads: AtomicUsize::new(0),
            seq_prefetch: Mutex::new(HashMap::new()),
            offline_until: Mutex::new(None),
            alive_at: Arc::new(Mutex::new(None)),
            events: Mutex::new(Vec::new()),
            last_counts: Mutex::new(UploadCounts::default()),
            background: Mutex::new(Vec::new()),
            opts,
        });
        // 유휴 블록 정리 작업. VFS가 사라지면 멈춥니다.
        let weak = Arc::downgrade(&s);
        s.spawn_background(async move {
            let mut tick = tokio::time::interval(BLOCK_TRIM_INTERVAL);
            loop {
                tick.tick().await;
                let Some(s) = weak.upgrade() else { break };
                s.blocks.trim_idle(BLOCK_IDLE);
                s.probes.trim_idle(BLOCK_IDLE);
            }
        });
        let recovered = s.recover();
        if recovered > 0 {
            tracing::info!(
                count = recovered,
                "이전 실행에서 남은 업로드를 이어서 올립니다"
            );
        }
        Ok(Self { s })
    }

    /// 마운트 해제 뒤 호출합니다. 진행 중 업로드를 멈추고 남은 변경분은 저널과 스테이징 파일로 남깁니다.
    pub fn shutdown(&self) {
        self.s.closed.store(true, Ordering::SeqCst);
        let mut tasks = std::mem::take(&mut *self.s.background.lock());
        for node in self.s.nodes.lock().values() {
            let mut st = node.state.lock();
            if let Some((_, task)) = st.upload.take() {
                tasks.push(task);
            }
            if let Some(stream) = st.stream.take() {
                stream.abort();
            }
        }
        for task in &tasks {
            task.abort();
        }
        // 해제 완료를 알리기 전에 백그라운드 작업의 백엔드 참조를 내려놓습니다.
        self.s.rt.block_on(async {
            while tasks.iter().any(|task| !task.is_finished()) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
    }

    pub fn read_only(&self) -> bool {
        self.s.opts.read_only
    }

    pub fn backend(&self) -> &Arc<dyn Backend> {
        &self.s.backend
    }

    fn writable(&self) -> Result<()> {
        if self.s.opts.read_only {
            Err(Error::ReadOnly)
        } else {
            Ok(())
        }
    }

    fn open_node(&self, path: &RemotePath) -> Option<Arc<Node>> {
        self.s.nodes.lock().get(path).cloned()
    }

    /// 실제 원격 경로(대소문자 보정)와 항목을 찾습니다.
    pub fn stat(&self, path: &RemotePath) -> Result<(RemotePath, Entry)> {
        self.s.refresh_metadata_epoch();
        if path.is_root() {
            return Ok((RemotePath::root(), Entry::dir("")));
        }
        if let Some(node) = self.open_node(path) {
            let st = node.state.lock();
            if !st.deleted {
                return Ok((path.clone(), st.entry.clone()));
            }
        }
        let s = &self.s;
        if let Some(e) = s.meta.entry(path) {
            return Ok((path.clone(), e));
        }
        match s.meta.lookup(path) {
            Lookup::Found(real, e) => return Ok((real, e)),
            Lookup::Missing => return Err(Error::NotFound),
            Lookup::Unknown => {}
        }
        let saved_dir = path.parent().and_then(|parent| {
            s.store
                .as_ref()
                .and_then(|st| st.load_dir(&parent).ok().flatten())
        });
        let saved_entry = s
            .store
            .as_ref()
            .and_then(|st| st.load_entry(path).ok().flatten());
        match s.rt.block_on(s.remote_read(
            s.backend.stat(path),
            saved_dir.is_some() || saved_entry.is_some(),
        )) {
            Ok(e) => {
                s.meta.put_entry(path, e.clone());
                if let Some(store) = &s.store
                    && let Err(err) = store.save_entry(path, &e)
                {
                    tracing::debug!(%path, error = %err, "항목 저장 실패");
                }
                Ok((path.clone(), e))
            }
            Err(Error::NotFound) if s.backend.capabilities().case_sensitive => {
                // 대소문자를 구분하는 원격에서는 부모 목록으로 실제 이름을 찾습니다.
                let (Some(parent), Some(name)) = (path.parent(), path.name()) else {
                    return Err(Error::NotFound);
                };
                let (real_parent, pe) = self.stat(&parent)?;
                if !pe.is_dir() {
                    return Err(Error::NotADirectory);
                }
                self.list(&real_parent)?;
                match s.meta.lookup(&real_parent.join(name)?) {
                    Lookup::Found(real, e) => Ok((real, e)),
                    _ => Err(Error::NotFound),
                }
            }
            Err(e) if offline(&e) => {
                let (Some(parent), Some(name)) = (path.parent(), path.name()) else {
                    return Err(e);
                };
                match saved_dir {
                    Some(list) => list
                        .into_iter()
                        .find(|x| x.name.eq_ignore_ascii_case(name))
                        .map(|x| (parent.join(&x.name).unwrap_or(path.clone()), x))
                        .ok_or(Error::NotFound),
                    // 목록을 받은 적이 없으면 단일 항목 조회 기록을 씁니다.
                    None => match saved_entry {
                        Some(x) => Ok((path.clone(), x)),
                        None => Err(e),
                    },
                }
            }
            Err(e) => Err(e),
        }
    }

    /// 디렉터리 목록. 열린 노드의 스테이징 크기와 시각을 반영합니다.
    pub fn list(&self, path: &RemotePath) -> Result<Arc<Vec<Entry>>> {
        let s = &self.s;
        s.refresh_metadata_epoch();
        s.watched.lock().insert(path.clone(), Instant::now());
        let list = match s.meta.dir(path) {
            Some(l) => l,
            None => {
                tracing::debug!(%path, "원격 폴더 목록 조회");
                let saved = s
                    .store
                    .as_ref()
                    .and_then(|st| st.load_dir(path).ok().flatten());
                match s
                    .rt
                    .block_on(s.remote_read(s.backend.list(path), saved.is_some()))
                {
                    Ok(l) => {
                        if let Some(store) = &s.store
                            && let Err(e) = store.save_dir(path, &l)
                        {
                            tracing::warn!(%path, error = %e, "메타 저장 실패");
                        }
                        let l = Arc::new(l);
                        s.meta.put_dir(path, l.clone());
                        l
                    }
                    Err(e) if offline(&e) => match saved {
                        Some(l) => {
                            tracing::warn!(%path, error = %e, "원격 응답 없음: 저장된 목록 사용");
                            Arc::new(l)
                        }
                        None => return Err(e),
                    },
                    Err(e) => return Err(e),
                }
            }
        };
        let overlay: Vec<(String, Entry, bool)> = {
            let nodes = s.nodes.lock();
            nodes
                .iter()
                .filter(|(p, _)| p.parent().as_ref() == Some(path))
                .filter_map(|(p, n)| {
                    let st = n.state.lock();
                    ((st.dirty || st.staged.is_some()) && !st.deleted).then(|| {
                        (
                            p.name().unwrap_or_default().to_owned(),
                            st.entry.clone(),
                            st.local_only,
                        )
                    })
                })
                .collect()
        };
        if overlay.is_empty() {
            return Ok(list);
        }
        let mut out = (*list).clone();
        for (name, e, local_only) in overlay {
            if let Some(slot) = out.iter_mut().find(|x| x.name == name) {
                *slot = e;
            } else if local_only {
                // 원격에 아직 없는 새 파일도 쓰는 동안 목록에 보여 줍니다.
                out.push(e);
            }
        }
        Ok(Arc::new(out))
    }

    fn attach(&self, path: RemotePath, entry: Entry) -> Arc<FileHandle> {
        let mut nodes = self.s.nodes.lock();
        let node = nodes
            .entry(path.clone())
            .or_insert_with(|| {
                Arc::new(Node {
                    kind: entry.kind,
                    path: Mutex::new(path),
                    opens: AtomicUsize::new(0),
                    upload_class: AtomicU8::new(UPLOAD_NONE),
                    state: Mutex::new(NodeState {
                        entry,
                        staged: None,
                        dirty: false,
                        generation: 0,
                        upload: None,
                        failures: 0,
                        pending_mtime: None,
                        deleted: false,
                        journaled: None,
                        base_etag: None,
                        recovered: false,
                        partial: None,
                        stream: None,
                        fresh: false,
                        fallback: false,
                        size_hint: None,
                        committing: false,
                        local_only: false,
                        transferring: false,
                    }),
                })
            })
            .clone();
        node.opens.fetch_add(1, Ordering::SeqCst);
        Arc::new(FileHandle {
            node,
            read_pattern: Mutex::new(read_pattern::ReadPattern::default()),
        })
    }

    pub fn open(&self, path: &RemotePath) -> Result<Arc<FileHandle>> {
        let (real, entry) = self.stat(path)?;
        Ok(self.attach(real, entry))
    }

    /// 새 파일(빈 파일 업로드) 또는 디렉터리를 만들고 엽니다.
    pub fn create(&self, path: &RemotePath, kind: EntryKind) -> Result<Arc<FileHandle>> {
        self.writable()?;
        match self.stat(path) {
            Ok(_) => return Err(Error::AlreadyExists),
            Err(Error::NotFound) => {}
            Err(e) => return Err(e),
        }
        if let Some(parent) = path.parent()
            && !self.stat(&parent)?.1.is_dir()
        {
            return Err(Error::NotADirectory);
        }
        let s = &self.s;
        // 삭제 표시만 남은 옛 노드를 새 항목이 이어받지 않게 합니다.
        {
            let mut nodes = s.nodes.lock();
            if nodes.get(path).is_some_and(|n| n.state.lock().deleted) {
                nodes.remove(path);
            }
        }
        if kind == EntryKind::File && self.through() {
            // 실시간 모드는 원격에 빈 파일을 먼저 만들지 않고 첫 쓰기나 닫기 때 한 번에 만듭니다.
            // 파일마다 원격 왕복이 한 세트 줄고, 다 쓰지 않은 0바이트 파일이 원격에 먼저 보이지 않습니다.
            let now = SystemTime::now();
            let mut entry = Entry::file(path.name().unwrap_or_default(), 0);
            entry.modified = Some(now);
            entry.created = Some(now);
            let staged = NamedTempFile::new_in(&s.opts.staging_dir)?;
            let h = self.attach(path.clone(), entry.clone());
            {
                let mut st = h.node.state.lock();
                st.reset_fresh(staged);
                st.local_only = true;
                st.dirty = true;
            }
            s.meta.upsert_child(path, entry);
            return Ok(h);
        }
        let entry = match kind {
            EntryKind::Dir => s.run(s.backend.mkdir(path))?,
            EntryKind::File => s.run(s.backend.write(path, WriteSource::Bytes(Bytes::new())))?,
        };
        s.meta.upsert_child(path, entry.clone());
        s.persist_entry(path, &entry);
        Ok(self.attach(path.clone(), entry))
    }

    pub fn info(&self, h: &FileHandle) -> Entry {
        h.node.state.lock().entry.clone()
    }

    pub fn read(&self, h: &FileHandle, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let entry = {
            let st = h.node.state.lock();
            if let Some(staged) = &st.staged {
                return read_staged(staged, st.entry.size, offset, buf);
            }
            st.entry.clone()
        };
        let size = entry.size;
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let s = &self.s;
        let path = h.path();
        let ver = version(&entry);
        let bs = s.blocks.block_size();
        let end = (offset + buf.len() as u64).min(size);
        let mut pos = offset;
        // 짧은 머리, 꼬리 조회는 4MiB 블록 대신 작은 조각만 받아 미리보기와 속성 표시를 빠르게 합니다.
        let probe = bs >= 2 * PROBE_CHUNK
            && buf.len() <= PROBE_MAX_READ
            && h.read_pattern.lock().continued(offset) + buf.len() as u64 <= PROBE_ESCALATE;
        tracing::trace!(%path, offset, len = buf.len(), probe, "읽기 요청");
        while pos < end {
            let (block, base) = if probe {
                s.fetch_probe(&path, &ver, pos, size)?
            } else {
                let idx = pos / bs;
                (s.fetch_block(&path, &ver, idx, size)?, idx * bs)
            };
            let in_off = (pos - base) as usize;
            if in_off >= block.len() {
                break;
            }
            let n = (block.len() - in_off).min((end - pos) as usize);
            let at = (pos - offset) as usize;
            buf[at..at + n].copy_from_slice(&block[in_off..in_off + n]);
            pos += n as u64;
        }
        let memory_window = (s.opts.memory_cache_bytes as u64 / bs / 2).min(16) as u32;
        let window = h.read_pattern.lock().observe(
            offset,
            pos,
            bs,
            s.opts.readahead_blocks.min(memory_window),
        );
        if window > 0 && pos > 0 {
            s.prefetch(&path, &ver, (pos - 1) / bs + 1, size, window);
        }
        Ok((pos - offset) as usize)
    }

    /// 변경 표시. 백그라운드 모드에서는 업로드 저널에도 기록합니다.
    ///
    /// 실시간 모드는 호출 프로그램이 닫기까지 완료를 확인하지 않았으므로 열린 파일을 저널에 남기지
    /// 않습니다. 남기면 다음 실행에서 불완전한 내용을 완성본처럼 올리게 됩니다.
    fn mark_dirty(&self, path: &RemotePath, st: &mut NodeState) {
        st.dirty = true;
        if self.s.opts.write_mode == WriteMode::Back {
            self.s.journal(path, st);
        }
    }

    fn through(&self) -> bool {
        self.s.opts.write_mode == WriteMode::Through
    }

    /// 스테이징 파일이 없으면 만들고 현재 원격 내용을 채웁니다.
    fn ensure_staged(&self, path: &RemotePath, st: &mut NodeState) -> Result<()> {
        if st.staged.is_some() {
            return Ok(());
        }
        let s = &self.s;
        let size = st.entry.size;
        let ver = version(&st.entry);
        let bs = s.blocks.block_size();
        let (mut tmp, mut done) = match st.partial.take() {
            Some((f, done, v)) if v == ver => {
                tracing::info!(%path, done, size, "끊긴 내려받기를 이어받습니다");
                (f, done)
            }
            _ => (NamedTempFile::new_in(&s.opts.staging_dir)?, 0),
        };
        st.fresh = size == 0;
        while done < size {
            let idx = done / bs;
            let block = match s.fetch_block(path, &ver, idx, size) {
                Ok(b) => b,
                Err(e) => {
                    st.partial = Some((tmp, done, ver));
                    return Err(e);
                }
            };
            let in_off = (done - idx * bs) as usize;
            if in_off >= block.len() {
                break;
            }
            tmp.write_all(&block[in_off..])?;
            done += (block.len() - in_off) as u64;
            // 원격이 줄어들어 블록이 짧으면 여기서 끝냅니다.
            if (block.len() as u64) < bs.min(size - idx * bs) {
                break;
            }
        }
        st.entry.size = done;
        st.staged = Some(tmp);
        Ok(())
    }

    /// `constrained`가 true면(페이징 I/O) 파일 크기를 늘리지 않습니다.
    pub fn write(
        &self,
        h: &FileHandle,
        offset: u64,
        data: &[u8],
        write_to_eof: bool,
        constrained: bool,
    ) -> Result<(usize, Entry)> {
        self.writable()?;
        let path = h.path();
        // 실시간 세션이 있으면 전송 창 자리를 잠금 밖에서 먼저 받습니다. 창이 찰 때만 여기서 기다리므로
        // 진행률이 실제 전송을 따르고, 기다리는 동안 같은 파일의 다른 요청과 폴더 조회는 막히지 않습니다.
        // 세션으로 보내기는 아래 잠금 안에서 해, 여러 스레드가 동시에 써도 조각과 확정 순서가 뒤바뀌지 않습니다.
        let reserved = if self.through() && !data.is_empty() {
            let window = h
                .node
                .state
                .lock()
                .stream
                .as_ref()
                .map(|s| s.window.clone());
            match window {
                Some(window) => {
                    let want = data.len().min(STREAM_WINDOW) as u32;
                    self.s
                        .run(async move {
                            window
                                .acquire_many_owned(want)
                                .await
                                .map_err(|_| Error::Cancelled)
                        })
                        .ok()
                }
                None => None,
            }
        } else {
            None
        };
        let mut st = h.node.state.lock();
        // 세션이 이미 실패했으면 호출 프로그램에 알리고 정리 때 전체 업로드로 전환합니다.
        if let Some(e) = st.stream.as_ref().and_then(StreamState::error) {
            st.drop_stream();
            return Err(e);
        }
        st.begin_change();
        self.ensure_staged(&path, &mut st)?;
        let size = st.entry.size;
        let off = if write_to_eof { size } else { offset };
        let data = if constrained {
            if off >= size {
                return Ok((0, st.entry.clone()));
            }
            &data[..data.len().min((size - off) as usize)]
        } else {
            data
        };
        let file = st.staged.as_ref().expect("staged").as_file();
        let mut done = 0;
        while done < data.len() {
            done += file.seek_write(&data[done..], off + done as u64)?;
        }
        st.entry.size = size.max(off + data.len() as u64);
        st.entry.modified = Some(SystemTime::now());
        self.mark_dirty(&path, &mut st);
        let entry = st.entry.clone();
        if !self.through() || data.is_empty() {
            return Ok((data.len(), entry));
        }

        // 실시간 모드: 세션을 열거나 이어서 보냅니다.
        if st.stream.is_none()
            && st.fresh
            && !st.fallback
            && !st.committing
            && self.s.backend.capabilities().streaming_upload
        {
            self.s.open_stream(&path, &mut st);
        }
        let Some(stream) = st.stream.as_mut() else {
            st.fallback = true;
            return Ok((data.len(), entry));
        };
        if !stream.random && off != stream.next {
            tracing::debug!(%path, off, expected = stream.next, "순서 밖 쓰기: 정리 때 전체 업로드");
            st.drop_stream();
            return Ok((data.len(), entry));
        }
        // 미리 받은 자리가 지금 세션의 창이면 쓰고, 그사이 세션이 바뀌었으면(첫 쓰기 포함) 새 창에서
        // 기다리지 않고 받습니다. 받을 수 없으면 순서를 지킬 수 없으므로 정리 때 전체 업로드로 넘깁니다.
        let want = data.len().min(STREAM_WINDOW) as u32;
        let permit = match reserved {
            Some(p) if Arc::ptr_eq(p.semaphore(), &stream.window) => Some(p),
            _ => stream.window.clone().try_acquire_many_owned(want).ok(),
        };
        let Some(permit) = permit else {
            tracing::debug!(%path, "전송 창이 가득 차 정리 때 전체 업로드");
            st.drop_stream();
            return Ok((data.len(), entry));
        };
        let end = off + data.len() as u64;
        stream.next = end;
        stream.sent += data.len() as u64;
        stream.max_end = stream.max_end.max(end);
        // 잠금을 쥔 채 보내 다른 스레드의 조각이나 확정 명령보다 앞서도록 순서를 고정합니다.
        let sent = stream
            .tx
            .send(StreamCmd::Write(off, Bytes::copy_from_slice(data), permit));
        if sent.is_err() {
            let e = stream.error().unwrap_or(Error::Cancelled);
            st.drop_stream();
            return Err(e);
        }
        let complete = st.size_hint.is_some_and(|n| {
            let stream = st.stream.as_ref().expect("stream");
            stream.sent == n && stream.max_end == n
        });
        drop(st);
        let s = &self.s;
        if complete {
            // 마지막 바이트가 도착했으므로 지금 확정합니다. 실패는 이 쓰기의 결과로 돌려줍니다.
            s.commit_now(&h.node, true)?;
        }
        Ok((data.len(), entry))
    }

    /// 파일 크기를 바꿉니다. 할당 크기 요청은 현재 크기보다 작을 때만 잘라냅니다.
    pub fn set_size(&self, h: &FileHandle, new_size: u64, allocation: bool) -> Result<Entry> {
        let mut st = h.node.state.lock();
        if (allocation && new_size >= st.entry.size) || (!allocation && new_size == st.entry.size) {
            return Ok(st.entry.clone());
        }
        self.writable()?;
        st.begin_change();
        let path = h.path();
        if new_size == 0 {
            st.reset_fresh(NamedTempFile::new_in(&self.s.opts.staging_dir)?);
        } else {
            self.ensure_staged(&path, &mut st)?;
            if self.through() {
                match &st.stream {
                    // 데이터를 보내기 전의 크기 설정은 최종 크기 힌트로 씁니다.
                    None if st.fresh && st.entry.size == 0 => st.size_hint = Some(new_size),
                    None => st.size_hint = None,
                    // 보내는 중의 크기 변경은 세션으로 표현할 수 없어 정리 때 전체 업로드로 전환합니다.
                    Some(_) => st.drop_stream(),
                }
            }
        }
        st.staged
            .as_ref()
            .expect("staged")
            .as_file()
            .set_len(new_size)?;
        st.entry.size = new_size;
        st.entry.modified = Some(SystemTime::now());
        self.mark_dirty(&path, &mut st);
        Ok(st.entry.clone())
    }

    /// 덮어쓰기 열기(CREATE_ALWAYS 등). 내용을 비웁니다.
    pub fn overwrite(&self, h: &FileHandle) -> Result<Entry> {
        self.set_size(h, 0, false)
    }

    pub fn set_mtime(&self, h: &FileHandle, mtime: SystemTime) -> Result<Entry> {
        let s = &self.s;
        let mtime = s.backend.normalize_mtime(mtime);
        let mut st = h.node.state.lock();
        if st.dirty {
            st.pending_mtime = Some(mtime);
            st.entry.modified = Some(mtime);
        } else if s.backend.capabilities().set_mtime && !s.opts.read_only {
            s.run(s.backend.set_mtime(&h.path(), mtime))?;
            st.entry.modified = Some(mtime);
            // 시각 변경 전의 버전 표식은 유효하지 않으며 다음 폴링에서 다시 확인합니다.
            st.entry.etag = None;
            s.meta.upsert_child(&h.path(), st.entry.clone());
            s.persist_entry(&h.path(), &st.entry);
        }
        Ok(st.entry.clone())
    }

    /// 삭제 표시 전에 호출합니다. 비어 있지 않은 디렉터리는 거부합니다.
    pub fn check_delete(&self, h: &FileHandle) -> Result<()> {
        self.writable()?;
        let path = h.path();
        if path.is_root() {
            return Err(Error::PermissionDenied);
        }
        if h.is_dir() && !self.list(&path)?.is_empty() {
            return Err(Error::DirectoryNotEmpty);
        }
        Ok(())
    }

    pub fn rename(&self, h: &FileHandle, to: &RemotePath, replace: bool) -> Result<()> {
        self.writable()?;
        let s = &self.s;
        let from = h.path();
        if from.is_root() || to.is_root() {
            return Err(Error::PermissionDenied);
        }
        // 대소문자를 구분하지 않는 볼륨에서 WinFsp는 대상의 상위 경로를 대문자로 넘깁니다.
        // 대소문자를 구분하는 원격(SFTP 등)에서도 찾을 수 있게 실제 상위 경로로 바꿉니다.
        let to = &self.real_target(to)?;
        if h.is_dir() && to.starts_with(&from) && !to.as_str().eq_ignore_ascii_case(from.as_str()) {
            return Err(Error::InvalidPath(
                "디렉터리를 자기 하위로 옮길 수 없습니다".into(),
            ));
        }
        if h.is_dir() && !s.backend.capabilities().rename_dir {
            return Err(Error::Unsupported("디렉터리 이름 변경"));
        }
        if !h.is_dir() && h.node.state.lock().local_only {
            return self.rename_local(h, &from, to, replace);
        }

        // 옛 경로로 가는 업로드를 멈추고 이름 변경 뒤 새 경로로 다시 올립니다.
        // 원격 호출 동안 노드 테이블 잠금을 쥐지 않도록 대상만 먼저 모읍니다.
        let mut requeue = Vec::new();
        for node in s.nodes.lock().iter().filter(|(p, _)| p.starts_with(&from)) {
            let mut st = node.1.state.lock();
            if st.upload.is_some() {
                st.begin_change();
                requeue.push(node.1.clone());
            }
            // 열린 세션은 옛 경로로 확정되므로 버리고 정리 때 새 경로로 전체 업로드합니다.
            if st.stream.is_some() {
                st.drop_stream();
            }
        }
        if let Err(e) = s.run(s.backend.rename(&from, to, replace)) {
            for node in requeue {
                s.schedule_upload(&node, Duration::ZERO);
            }
            return Err(e);
        }
        let mut nodes = s.nodes.lock();
        let moved: Vec<RemotePath> = nodes
            .keys()
            .filter(|p| p.starts_with(&from))
            .cloned()
            .collect();
        for old in moved {
            let Some(new) = old.rebase(&from, to) else {
                continue;
            };
            if let Some(node) = nodes.remove(&old) {
                node.state.lock().entry.name = new.name().unwrap_or_default().to_owned();
                *node.path.lock() = new.clone();
                nodes.insert(new, node);
            }
        }
        drop(nodes);
        if let Some(store) = &s.store
            && let Err(e) = store.journal_move(&from, to)
        {
            tracing::warn!(%from, %to, error = %e, "업로드 저널 경로 갱신 실패");
        }
        s.forget(&from);
        s.forget(to);
        let mut moved_entry = h.node.state.lock().entry.clone();
        moved_entry.name = to.name().unwrap_or_default().to_owned();
        s.persist_entry(to, &moved_entry);
        for node in requeue {
            if node.opens.load(Ordering::SeqCst) == 0 {
                s.schedule_upload(&node, Duration::ZERO);
            }
        }
        Ok(())
    }

    /// 원격에 아직 없는 새 파일의 이름 변경. 원격 호출 없이 노드만 옮기고 확정 때 새 경로로 올립니다.
    fn rename_local(
        &self,
        h: &FileHandle,
        from: &RemotePath,
        to: &RemotePath,
        replace: bool,
    ) -> Result<()> {
        let s = &self.s;
        if !to.as_str().eq_ignore_ascii_case(from.as_str()) {
            match self.stat(to) {
                Ok((_, e)) if e.is_dir() => return Err(Error::IsADirectory),
                Ok(_) if !replace => return Err(Error::AlreadyExists),
                Ok(_) | Err(Error::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        {
            let mut st = h.node.state.lock();
            // 열린 세션은 옛 경로로 확정되므로 버리고 정리 때 새 경로로 올립니다.
            if st.stream.is_some() {
                st.drop_stream();
            }
            st.entry.name = to.name().unwrap_or_default().to_owned();
        }
        {
            let mut nodes = s.nodes.lock();
            if let Some(node) = nodes.remove(from) {
                *node.path.lock() = to.clone();
                nodes.insert(to.clone(), node);
            }
        }
        s.meta.invalidate(from);
        s.meta.invalidate(to);
        Ok(())
    }

    /// 상위 경로를 원격의 실제 대소문자로 맞춘 대상 경로. 마지막 이름은 요청 그대로 둡니다.
    fn real_target(&self, to: &RemotePath) -> Result<RemotePath> {
        let (Some(parent), Some(name)) = (to.parent(), to.name()) else {
            return Ok(to.clone());
        };
        let (real, entry) = self.stat(&parent)?;
        if !entry.is_dir() {
            return Err(Error::NotADirectory);
        }
        real.join(name)
    }

    /// 핸들 정리. `delete`면 원격에서 삭제합니다. 마지막 핸들이면 실시간 모드는 변경분을 지금 원격에
    /// 반영하고 돌아오며, 백그라운드 모드는 업로드를 예약합니다.
    pub fn cleanup(&self, h: &FileHandle, delete: bool) -> Result<()> {
        let remaining = h.node.opens.fetch_sub(1, Ordering::SeqCst) - 1;
        let result = if delete { self.delete_node(h) } else { Ok(()) };
        if remaining == 0 {
            self.settle(&h.node);
        }
        result
    }

    /// 핸들 해제. Cleanup 뒤 페이징 쓰기로 다시 변경된 내용이 있으면 반영합니다.
    /// 정리 단계의 커밋이 실패해 백그라운드 재시도를 예약했으면 여기서 다시 기다리지 않습니다.
    pub fn close(&self, h: &FileHandle) {
        if h.node.opens.load(Ordering::SeqCst) != 0 {
            return;
        }
        let retry_pending = self.through() && {
            let st = h.node.state.lock();
            st.upload.is_some() || st.failures > 0
        };
        if retry_pending {
            self.s.release(&h.node);
        } else {
            self.settle(&h.node);
        }
    }

    fn settle(&self, node: &Arc<Node>) {
        if self.through() {
            // 실패는 알림과 저널로 처리했습니다. 정리 요청 자체는 성공으로 돌려줍니다.
            let _ = self.s.commit_now(node, false);
        } else {
            self.s.schedule_upload(node, Duration::ZERO);
        }
        self.s.release(node);
    }

    /// 실시간 모드에서는 지금까지 보낸 데이터가 원격에 수락될 때까지 기다립니다.
    pub fn flush(&self, h: &FileHandle) -> Result<Entry> {
        let wait = {
            let st = h.node.state.lock();
            st.stream.as_ref().map(|stream| {
                let (reply, wait) = oneshot::channel();
                (stream.tx.send(StreamCmd::Flush(reply)).is_ok(), wait)
            })
        };
        if let Some((sent, wait)) = wait {
            let r = if sent {
                self.s
                    .run(async { wait.await.unwrap_or(Err(Error::Cancelled)) })
            } else {
                Err(Error::Cancelled)
            };
            if let Err(e) = r {
                let mut st = h.node.state.lock();
                let e = st.stream.as_ref().and_then(StreamState::error).unwrap_or(e);
                st.drop_stream();
                return Err(e);
            }
        }
        Ok(self.info(h))
    }

    fn delete_node(&self, h: &FileHandle) -> Result<()> {
        self.writable()?;
        let s = &self.s;
        let path = h.path();
        let mut st = h.node.state.lock();
        st.begin_change();
        if let Some(stream) = st.stream.take() {
            stream.abort();
        }
        // 원격에 아직 없는 새 파일은 로컬 상태만 지웁니다.
        if !st.local_only {
            s.run(s.backend.delete(&path, h.node.kind))?;
        }
        st.staged = None;
        st.dirty = false;
        st.deleted = true;
        if st.journaled.take().is_some()
            && let Some(store) = &s.store
        {
            let _ = store.journal_remove(&path);
        }
        drop(st);
        if h.is_dir() {
            s.forget(&path);
        } else {
            s.meta.remove_child(&path);
            s.drop_blocks(&path);
            if let Some(store) = &s.store
                && let Err(e) = store.forget_tree(&path)
            {
                tracing::warn!(%path, error = %e, "메타 저장소 정리 실패");
            }
        }
        Ok(())
    }

    pub fn take_transfer_events(&self) -> Vec<TransferEvent> {
        std::mem::take(&mut *self.s.events.lock())
    }

    /// 원격에서 받고 있는 블록 수
    pub fn active_downloads(&self) -> usize {
        self.s.downloads.load(Ordering::SeqCst)
    }

    /// 업로드 대기 중이거나 진행 중인 파일 수
    pub fn pending_uploads(&self) -> usize {
        self.s
            .nodes
            .lock()
            .values()
            .filter(|n| {
                let st = n.state.lock();
                st.dirty && !st.deleted
            })
            .count()
    }

    /// 업로드가 끝나지 않은 파일 수를 전송 중, 대기, 실패로 나눕니다. 트레이와 화면 표시에 씁니다.
    ///
    /// 에이전트가 tokio 작업 스레드에서 주기적으로 부르므로 어떤 잠금에서도 기다리지 않습니다.
    /// 파일 시스템 요청이 상태 잠금을 쥔 채 원격 응답을 기다리는 동안 여기서 막히면, 그 작업 스레드가
    /// 맡던 네트워크 이벤트 처리까지 멈춰 원격 응답이 영영 오지 않는 교착이 생깁니다.
    /// 잠금이 바쁜 파일은 마지막으로 확인한 상태를, 노드 표가 바쁘면 직전 집계를 씁니다.
    pub fn upload_counts(&self) -> UploadCounts {
        let through = self.through();
        let Some(nodes) = self.s.nodes.try_lock() else {
            return *self.s.last_counts.lock();
        };
        let mut c = UploadCounts::default();
        for n in nodes.values() {
            let class = match n.state.try_lock() {
                Some(st) => {
                    let class = if !st.dirty || st.deleted {
                        UPLOAD_NONE
                    } else if st.stream.is_some()
                        || st.committing
                        || st.transferring
                        || (through && n.opens.load(Ordering::SeqCst) > 0)
                    {
                        UPLOAD_ACTIVE
                    } else if st.failures > 0 {
                        UPLOAD_FAILED
                    } else {
                        UPLOAD_WAITING
                    };
                    n.upload_class.store(class, Ordering::Relaxed);
                    class
                }
                None => n.upload_class.load(Ordering::Relaxed),
            };
            match class {
                UPLOAD_ACTIVE => c.uploading += 1,
                UPLOAD_WAITING => c.waiting += 1,
                UPLOAD_FAILED => c.failed += 1,
                _ => {}
            }
        }
        drop(nodes);
        *self.s.last_counts.lock() = c;
        c
    }

    /// 모든 변경분을 업로드할 때까지 기다립니다. 마운트 해제 전에 호출합니다.
    pub fn sync(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let pending: Vec<Arc<Node>> = self
                .s
                .nodes
                .lock()
                .values()
                .filter(|n| {
                    let st = n.state.lock();
                    st.dirty && !st.deleted
                })
                .cloned()
                .collect();
            if pending.is_empty() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let names: Vec<String> =
                    pending.iter().map(|n| n.path.lock().to_string()).collect();
                return Err(Error::Protocol(format!(
                    "업로드가 끝나지 않았습니다: {}",
                    names.join(", ")
                )));
            }
            for node in &pending {
                let exhausted = {
                    let mut st = node.state.lock();
                    let ex = st.failures > MAX_UPLOAD_RETRIES;
                    if ex {
                        st.failures = 0;
                    }
                    ex
                };
                if exhausted || node.opens.load(Ordering::SeqCst) == 0 {
                    self.s.schedule_upload(node, Duration::ZERO);
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn quota(&self) -> Quota {
        let mut q = self.s.quota.lock();
        if let Some((v, at)) = *q
            && at.elapsed() < QUOTA_TTL
        {
            return v;
        }
        let v = self
            .s
            .rt
            .block_on(self.s.remote_read(self.s.backend.quota(), true))
            .unwrap_or_default();
        *q = Some((v, Instant::now()));
        v
    }

    /// 주기적으로 원격 변경을 확인하는 백그라운드 작업을 시작합니다. VFS가 사라지면 멈춥니다.
    pub fn start_watch(&self, interval: Duration) {
        if interval.is_zero() {
            return;
        }
        let weak = Arc::downgrade(&self.s);
        self.s.spawn_background(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(s) = weak.upgrade() else { break };
                let found = s.poll_changes().await;
                if !found.is_empty() {
                    let mut q = s.changes.lock();
                    q.extend(found);
                    let excess = q.len().saturating_sub(MAX_PENDING_CHANGES);
                    q.drain(..excess);
                }
            }
        });
    }

    /// 지금 한 번 원격 변경을 확인합니다.
    pub fn poll_changes(&self) -> Vec<Change> {
        self.s.rt.block_on(self.s.poll_changes())
    }

    /// 백그라운드 확인으로 쌓인 변경을 꺼냅니다.
    pub fn take_changes(&self) -> Vec<Change> {
        std::mem::take(&mut *self.s.changes.lock())
    }

    /// 원격 변경을 다시 읽도록 메타 캐시를 비웁니다.
    pub fn refresh(&self) {
        self.s.meta.clear();
    }
}

fn read_staged(staged: &NamedTempFile, size: u64, offset: u64, buf: &mut [u8]) -> Result<usize> {
    if offset >= size {
        return Ok(0);
    }
    let want = buf.len().min((size - offset) as usize);
    let mut done = 0;
    while done < want {
        let n = staged
            .as_file()
            .seek_read(&mut buf[done..want], offset + done as u64)?;
        if n == 0 {
            break;
        }
        done += n;
    }
    Ok(done)
}

#[cfg(test)]
mod tests;

/// `이름 (ExpanDrv 충돌 2026-11-06 153012).확장자` 형태의 충돌 사본 경로(UTC)
fn conflict_path(path: &RemotePath, now: SystemTime) -> Result<RemotePath> {
    let name = path.name().unwrap_or_default();
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    let secs = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // 유닉스 일수를 그레고리력 날짜로 바꿉니다(H. Hinnant civil_from_days).
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let stamp = format!(
        "{y:04}-{m:02}-{d:02} {:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    );
    let parent = path.parent().unwrap_or_else(RemotePath::root);
    parent.join(&format!("{stem} (ExpanDrv 충돌 {stamp}){ext}"))
}
