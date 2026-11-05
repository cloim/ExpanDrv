//! 파일 시스템 드라이버 콜백과 [`Backend`] 사이의 동기 VFS 계층.
//!
//! WinFsp 디스패처 스레드에서 호출하므로 공개 API는 동기 함수이고, 내부에서 tokio 런타임으로
//! 백엔드 비동기 호출을 실행합니다. tokio 작업 스레드 안에서 호출하면 안 됩니다.
//!
//! - 읽기: 고정 크기 블록 단위로 받아 메모리 LRU에 두고, 순차 읽기면 다음 블록을 미리 받습니다.
//! - 쓰기: 로컬 스테이징 파일에 기록하고 마지막 핸들 정리 후 백그라운드에서 업로드합니다(write-back).
//!   업로드 중 내용이 다시 바뀌면 진행 중 업로드를 취소하고 다음 정리 때 다시 올립니다.
//! - 메타: 메모리 TTL 캐시와 SQLite 저장소. 원격이 응답하지 않으면 저장된 목록으로 탐색을 이어갑니다.

mod block;
mod meta;
mod persist;

use block::BlockCache;
use bytes::Bytes;
use expandrv_core::{Backend, Entry, EntryKind, Error, Quota, RemotePath, Result, WriteSource};
use meta::{Lookup, MetaCache};
use parking_lot::Mutex;
use persist::MetaStore;
use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::os::windows::fs::FileExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;
use tokio::sync::Semaphore;
use tokio::task::AbortHandle;

/// 스테이징을 채울 때 한 번에 내려받는 크기
const DOWNLOAD_CHUNK: u64 = 8 << 20;
const QUOTA_TTL: Duration = Duration::from_secs(60);
/// 자동 재시도 횟수. 넘으면 다음 정리나 `sync` 때 다시 시도합니다.
const MAX_UPLOAD_RETRIES: u32 = 5;

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
            readahead_blocks: 2,
            meta_db: None,
            max_parallel_uploads: 4,
        }
    }
}

#[derive(Clone)]
pub struct Vfs {
    s: Arc<Shared>,
}

struct Shared {
    backend: Arc<dyn Backend>,
    rt: tokio::runtime::Handle,
    opts: VfsOptions,
    meta: MetaCache,
    store: Option<MetaStore>,
    blocks: BlockCache,
    nodes: Mutex<HashMap<RemotePath, Arc<Node>>>,
    quota: Mutex<Option<(Quota, Instant)>>,
    uploads: Semaphore,
}

/// 열린 파일 또는 디렉터리 핸들. 같은 경로의 핸들은 하나의 노드를 공유합니다.
pub struct FileHandle {
    node: Arc<Node>,
    /// 마지막 읽기 끝 위치(순차 읽기 감지)
    last_read_end: AtomicU64,
}

struct Node {
    kind: EntryKind,
    path: Mutex<RemotePath>,
    /// Cleanup 전인 핸들 수
    opens: AtomicUsize,
    state: Mutex<NodeState>,
}

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
}

impl NodeState {
    /// 내용이 바뀌기 직전에 호출합니다. 진행 중 업로드를 취소합니다.
    fn begin_change(&mut self) {
        if let Some((_, h)) = self.upload.take() {
            h.abort();
        }
        self.generation += 1;
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
    fn run<T>(&self, fut: impl Future<Output = Result<T>>) -> Result<T> {
        self.rt.block_on(async {
            tokio::time::timeout(self.opts.op_timeout, fut)
                .await
                .unwrap_or(Err(Error::Timeout))
        })
    }

    fn forget(&self, path: &RemotePath) {
        self.meta.invalidate_tree(path);
        self.blocks.invalidate(path);
        if let Some(store) = &self.store {
            if let Err(e) = store.forget_tree(path) {
                tracing::warn!(%path, error = %e, "메타 저장소 정리 실패");
            }
        }
    }

    /// 정리가 끝나고 업로드도 없는 노드를 핸들 테이블에서 뺍니다.
    fn release(&self, node: &Arc<Node>) {
        let mut nodes = self.nodes.lock();
        if node.opens.load(Ordering::SeqCst) != 0 {
            return;
        }
        {
            let st = node.state.lock();
            if (st.dirty && !st.deleted) || st.upload.is_some() {
                return;
            }
        }
        let path = node.path.lock().clone();
        if nodes.get(&path).is_some_and(|n| Arc::ptr_eq(n, node)) {
            nodes.remove(&path);
        }
    }

    fn schedule_upload(self: &Arc<Self>, node: &Arc<Node>, delay: Duration) {
        let mut st = node.state.lock();
        if !st.dirty || st.deleted || st.upload.is_some() {
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
                let st = n.state.lock();
                match &st.staged {
                    Some(staged) if st.generation == generation && !st.deleted => Some((
                        n.path.lock().clone(),
                        WriteSource::File {
                            path: staged.path().to_owned(),
                            size: st.entry.size,
                        },
                        st.pending_mtime,
                    )),
                    _ => None,
                }
            };
            let Some((path, source, mtime)) = job else {
                s.upload_done(&n, generation, None);
                return;
            };
            let result = async {
                let mut e = s.backend.write(&path, source).await?;
                if let Some(m) = mtime.filter(|_| s.backend.capabilities().set_mtime) {
                    s.backend.set_mtime(&path, m).await?;
                    e.modified = Some(m);
                }
                Ok(e)
            }
            .await;
            s.upload_done(&n, generation, Some((path, result)));
        });
        st.upload = Some((generation, task.abort_handle()));
    }

    fn upload_done(
        self: &Arc<Self>,
        node: &Arc<Node>,
        generation: u64,
        outcome: Option<(RemotePath, Result<Entry>)>,
    ) {
        let mut st = node.state.lock();
        // 취소된 이전 작업이 늦게 끝난 경우 현재 업로드 상태를 건드리지 않습니다.
        if !matches!(st.upload, Some((g, _)) if g == generation) {
            return;
        }
        st.upload = None;
        match outcome {
            Some((path, Ok(entry))) if st.generation == generation && !st.deleted => {
                tracing::debug!(%path, size = entry.size, "업로드 완료");
                st.entry = entry.clone();
                st.dirty = false;
                st.staged = None;
                st.pending_mtime = None;
                st.failures = 0;
                self.meta.invalidate(&path);
                self.meta.put_entry(&path, entry);
                self.blocks.invalidate(&path);
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
            let first = node.state.lock().failures == 0;
            self.schedule_upload(node, if first { Duration::ZERO } else { delay });
        }
        self.release(node);
    }

    fn fetch_block(&self, path: &RemotePath, ver: &str, idx: u64, size: u64) -> Result<Bytes> {
        if let Some(b) = self.blocks.get(path, ver, idx) {
            return Ok(b);
        }
        let bs = self.blocks.block_size();
        let (cell, _) = self.blocks.inflight(path, idx);
        let got = self.run(async {
            Ok(cell
                .get_or_init(|| async {
                    let off = idx * bs;
                    let r = self.backend.read_range(path, off, bs.min(size - off)).await;
                    r.ok()
                })
                .await
                .clone())
        });
        self.blocks.finish_inflight(path, idx);
        match got? {
            Some(b) => {
                self.blocks.put(path, ver, idx, b.clone());
                Ok(b)
            }
            // 공유 셀에는 오류를 담지 않으므로 직접 다시 요청해 원래 오류를 돌려줍니다.
            None => {
                let off = idx * bs;
                self.run(self.backend.read_range(path, off, bs.min(size - off)))
            }
        }
    }

    fn prefetch(self: &Arc<Self>, path: &RemotePath, ver: &str, from: u64, size: u64) {
        let bs = self.blocks.block_size();
        for idx in from..from + self.opts.readahead_blocks as u64 {
            if idx * bs >= size || self.blocks.get(path, ver, idx).is_some() {
                continue;
            }
            let (cell, created) = self.blocks.inflight(path, idx);
            if !created {
                continue;
            }
            let (s, path, ver) = (self.clone(), path.clone(), ver.to_owned());
            self.rt.spawn(async move {
                let off = idx * bs;
                let data = cell
                    .get_or_init(|| async {
                        s.backend
                            .read_range(&path, off, bs.min(size - off))
                            .await
                            .ok()
                    })
                    .await
                    .clone();
                if let Some(b) = data {
                    s.blocks.put(&path, &ver, idx, b);
                }
                s.blocks.finish_inflight(&path, idx);
            });
        }
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
        Ok(Self {
            s: Arc::new(Shared {
                backend,
                rt,
                meta: MetaCache::new(opts.metadata_ttl),
                store,
                blocks: BlockCache::new(opts.block_size, opts.memory_cache_bytes),
                nodes: Mutex::new(HashMap::new()),
                quota: Mutex::new(None),
                uploads: Semaphore::new(opts.max_parallel_uploads.max(1)),
                opts,
            }),
        })
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
        match s.run(s.backend.stat(path)) {
            Ok(e) => {
                s.meta.put_entry(path, e.clone());
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
                let saved = s
                    .store
                    .as_ref()
                    .and_then(|st| st.load_dir(&parent).ok().flatten());
                match saved {
                    Some(list) => list
                        .into_iter()
                        .find(|x| x.name.eq_ignore_ascii_case(name))
                        .map(|x| (parent.join(&x.name).unwrap_or(path.clone()), x))
                        .ok_or(Error::NotFound),
                    None => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// 디렉터리 목록. 열린 노드의 스테이징 크기와 시각을 반영합니다.
    pub fn list(&self, path: &RemotePath) -> Result<Arc<Vec<Entry>>> {
        let s = &self.s;
        let list = match s.meta.dir(path) {
            Some(l) => l,
            None => match s.run(s.backend.list(path)) {
                Ok(l) => {
                    if let Some(store) = &s.store {
                        if let Err(e) = store.save_dir(path, &l) {
                            tracing::warn!(%path, error = %e, "메타 저장 실패");
                        }
                    }
                    let l = Arc::new(l);
                    s.meta.put_dir(path, l.clone());
                    l
                }
                Err(e) if offline(&e) => {
                    let saved = s
                        .store
                        .as_ref()
                        .and_then(|st| st.load_dir(path).ok().flatten());
                    match saved {
                        Some(l) => {
                            tracing::warn!(%path, error = %e, "원격 응답 없음: 저장된 목록 사용");
                            Arc::new(l)
                        }
                        None => return Err(e),
                    }
                }
                Err(e) => return Err(e),
            },
        };
        let overlay: Vec<(String, Entry)> = {
            let nodes = s.nodes.lock();
            nodes
                .iter()
                .filter(|(p, _)| p.parent().as_ref() == Some(path))
                .filter_map(|(p, n)| {
                    let st = n.state.lock();
                    (st.dirty || st.staged.is_some())
                        .then(|| (p.name().unwrap_or_default().to_owned(), st.entry.clone()))
                })
                .collect()
        };
        if overlay.is_empty() {
            return Ok(list);
        }
        let mut out = (*list).clone();
        for (name, e) in overlay {
            if let Some(slot) = out.iter_mut().find(|x| x.name == name) {
                *slot = e;
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
                    state: Mutex::new(NodeState {
                        entry,
                        staged: None,
                        dirty: false,
                        generation: 0,
                        upload: None,
                        failures: 0,
                        pending_mtime: None,
                        deleted: false,
                    }),
                })
            })
            .clone();
        node.opens.fetch_add(1, Ordering::SeqCst);
        Arc::new(FileHandle {
            node,
            last_read_end: AtomicU64::new(u64::MAX),
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
        if let Some(parent) = path.parent() {
            if !self.stat(&parent)?.1.is_dir() {
                return Err(Error::NotADirectory);
            }
        }
        let s = &self.s;
        let entry = match kind {
            EntryKind::Dir => s.run(s.backend.mkdir(path))?,
            EntryKind::File => s.run(s.backend.write(path, WriteSource::Bytes(Bytes::new())))?,
        };
        s.meta.invalidate(path);
        s.meta.put_entry(path, entry.clone());
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
        while pos < end {
            let idx = pos / bs;
            let block = s.fetch_block(&path, &ver, idx, size)?;
            let in_off = (pos - idx * bs) as usize;
            if in_off >= block.len() {
                break;
            }
            let n = (block.len() - in_off).min((end - pos) as usize);
            let at = (pos - offset) as usize;
            buf[at..at + n].copy_from_slice(&block[in_off..in_off + n]);
            pos += n as u64;
        }
        let prev_end = h.last_read_end.swap(pos, Ordering::Relaxed);
        if prev_end == offset || offset == 0 {
            s.prefetch(&path, &ver, (pos - 1) / bs + 1, size);
        }
        Ok((pos - offset) as usize)
    }

    /// 스테이징 파일이 없으면 만들고 현재 원격 내용을 채웁니다.
    fn ensure_staged(&self, path: &RemotePath, st: &mut NodeState) -> Result<()> {
        if st.staged.is_some() {
            return Ok(());
        }
        let s = &self.s;
        let mut tmp = NamedTempFile::new_in(&s.opts.staging_dir)?;
        let size = st.entry.size;
        let mut off = 0;
        while off < size {
            let chunk = s.run(
                s.backend
                    .read_range(path, off, DOWNLOAD_CHUNK.min(size - off)),
            )?;
            if chunk.is_empty() {
                break;
            }
            tmp.write_all(&chunk)?;
            off += chunk.len() as u64;
        }
        st.entry.size = off;
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
        let mut st = h.node.state.lock();
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
        st.dirty = true;
        Ok((data.len(), st.entry.clone()))
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
            st.staged = Some(NamedTempFile::new_in(&self.s.opts.staging_dir)?);
        } else {
            self.ensure_staged(&path, &mut st)?;
        }
        st.staged
            .as_ref()
            .expect("staged")
            .as_file()
            .set_len(new_size)?;
        st.entry.size = new_size;
        st.entry.modified = Some(SystemTime::now());
        st.dirty = true;
        Ok(st.entry.clone())
    }

    /// 덮어쓰기 열기(CREATE_ALWAYS 등). 내용을 비웁니다.
    pub fn overwrite(&self, h: &FileHandle) -> Result<Entry> {
        self.set_size(h, 0, false)
    }

    pub fn set_mtime(&self, h: &FileHandle, mtime: SystemTime) -> Result<Entry> {
        let s = &self.s;
        let mut st = h.node.state.lock();
        if st.dirty {
            st.pending_mtime = Some(mtime);
            st.entry.modified = Some(mtime);
        } else if s.backend.capabilities().set_mtime && !s.opts.read_only {
            s.run(s.backend.set_mtime(&h.path(), mtime))?;
            st.entry.modified = Some(mtime);
            s.meta.invalidate(&h.path());
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
        if h.is_dir() && to.starts_with(&from) && !to.as_str().eq_ignore_ascii_case(from.as_str()) {
            return Err(Error::InvalidPath(
                "디렉터리를 자기 하위로 옮길 수 없습니다".into(),
            ));
        }
        if h.is_dir() && !s.backend.capabilities().rename_dir {
            return Err(Error::Unsupported("디렉터리 이름 변경"));
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
        s.forget(&from);
        s.forget(to);
        for node in requeue {
            if node.opens.load(Ordering::SeqCst) == 0 {
                s.schedule_upload(&node, Duration::ZERO);
            }
        }
        Ok(())
    }

    /// 핸들 정리. `delete`면 원격에서 삭제하고, 마지막 핸들이면 변경분 업로드를 예약합니다.
    pub fn cleanup(&self, h: &FileHandle, delete: bool) -> Result<()> {
        let remaining = h.node.opens.fetch_sub(1, Ordering::SeqCst) - 1;
        let result = if delete { self.delete_node(h) } else { Ok(()) };
        if remaining == 0 {
            self.s.schedule_upload(&h.node, Duration::ZERO);
            self.s.release(&h.node);
        }
        result
    }

    /// 핸들 해제. Cleanup 뒤 페이징 쓰기로 다시 변경된 내용이 있으면 업로드를 예약합니다.
    pub fn close(&self, h: &FileHandle) {
        if h.node.opens.load(Ordering::SeqCst) == 0 {
            self.s.schedule_upload(&h.node, Duration::ZERO);
            self.s.release(&h.node);
        }
    }

    pub fn flush(&self, h: &FileHandle) -> Result<Entry> {
        Ok(self.info(h))
    }

    fn delete_node(&self, h: &FileHandle) -> Result<()> {
        self.writable()?;
        let s = &self.s;
        let path = h.path();
        let mut st = h.node.state.lock();
        st.begin_change();
        s.run(s.backend.delete(&path, h.node.kind))?;
        st.staged = None;
        st.dirty = false;
        st.deleted = true;
        drop(st);
        s.forget(&path);
        Ok(())
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
        if let Some((v, at)) = *q {
            if at.elapsed() < QUOTA_TTL {
                return v;
            }
        }
        let v = self.s.run(self.s.backend.quota()).unwrap_or_default();
        *q = Some((v, Instant::now()));
        v
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
