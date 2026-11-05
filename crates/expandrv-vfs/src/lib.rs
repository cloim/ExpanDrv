//! 파일 시스템 드라이버 콜백과 [`Backend`] 사이의 동기 VFS 계층.
//!
//! WinFsp 디스패처 스레드에서 호출하므로 공개 API는 동기 함수이고, 내부에서 tokio 런타임으로
//! 백엔드 비동기 호출을 실행합니다. tokio 작업 스레드 안에서 호출하면 안 됩니다.
//!
//! 쓰기는 로컬 스테이징 파일에 먼저 기록하고, 마지막 핸들의 Cleanup 또는 Close에서 업로드합니다.

mod meta;

use bytes::Bytes;
use expandrv_core::{Backend, Entry, EntryKind, Error, Quota, RemotePath, Result, WriteSource};
use meta::{Lookup, MetaCache};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::future::Future;
use std::io::Write;
use std::os::windows::fs::FileExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};
use tempfile::NamedTempFile;

/// 스테이징을 채울 때 한 번에 내려받는 크기
const DOWNLOAD_CHUNK: u64 = 8 << 20;
const QUOTA_TTL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct VfsOptions {
    pub read_only: bool,
    pub metadata_ttl: Duration,
    /// 쓰기 스테이징 파일 위치. 소유자만 접근 가능한 폴더여야 합니다.
    pub staging_dir: PathBuf,
    /// 백엔드 호출 한 번의 시간 상한
    pub op_timeout: Duration,
}

pub struct Vfs {
    backend: Arc<dyn Backend>,
    rt: tokio::runtime::Handle,
    opts: VfsOptions,
    meta: MetaCache,
    nodes: Mutex<HashMap<RemotePath, Arc<Node>>>,
    quota: Mutex<Option<(Quota, Instant)>>,
}

/// 열린 파일 또는 디렉터리 핸들. 같은 경로의 핸들은 하나의 노드를 공유합니다.
pub struct FileHandle {
    node: Arc<Node>,
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
    /// 업로드 후 적용할 사용자 지정 수정 시각
    pending_mtime: Option<SystemTime>,
    deleted: bool,
}

impl FileHandle {
    pub fn path(&self) -> RemotePath {
        self.node.path.lock().clone()
    }

    pub fn is_dir(&self) -> bool {
        self.node.kind == EntryKind::Dir
    }
}

impl Vfs {
    pub fn new(
        backend: Arc<dyn Backend>,
        rt: tokio::runtime::Handle,
        opts: VfsOptions,
    ) -> Result<Self> {
        std::fs::create_dir_all(&opts.staging_dir)?;
        Ok(Self {
            backend,
            rt,
            meta: MetaCache::new(opts.metadata_ttl),
            opts,
            nodes: Mutex::new(HashMap::new()),
            quota: Mutex::new(None),
        })
    }

    pub fn read_only(&self) -> bool {
        self.opts.read_only
    }

    pub fn backend(&self) -> &Arc<dyn Backend> {
        &self.backend
    }

    fn run<T>(&self, fut: impl Future<Output = Result<T>>) -> Result<T> {
        self.rt.block_on(async {
            tokio::time::timeout(self.opts.op_timeout, fut)
                .await
                .unwrap_or(Err(Error::Timeout))
        })
    }

    fn writable(&self) -> Result<()> {
        if self.opts.read_only {
            Err(Error::ReadOnly)
        } else {
            Ok(())
        }
    }

    fn open_node(&self, path: &RemotePath) -> Option<Arc<Node>> {
        self.nodes.lock().get(path).cloned()
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
        if let Some(e) = self.meta.entry(path) {
            return Ok((path.clone(), e));
        }
        match self.meta.lookup(path) {
            Lookup::Found(real, e) => return Ok((real, e)),
            Lookup::Missing => return Err(Error::NotFound),
            Lookup::Unknown => {}
        }
        match self.run(self.backend.stat(path)) {
            Ok(e) => {
                self.meta.put_entry(path, e.clone());
                Ok((path.clone(), e))
            }
            Err(Error::NotFound) if self.backend.capabilities().case_sensitive => {
                // 대소문자를 구분하는 원격에서는 부모 목록으로 실제 이름을 찾습니다.
                let (Some(parent), Some(name)) = (path.parent(), path.name()) else {
                    return Err(Error::NotFound);
                };
                let (real_parent, pe) = self.stat(&parent)?;
                if !pe.is_dir() {
                    return Err(Error::NotADirectory);
                }
                self.list(&real_parent)?;
                match self.meta.lookup(&real_parent.join(name)?) {
                    Lookup::Found(real, e) => Ok((real, e)),
                    _ => Err(Error::NotFound),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// 디렉터리 목록. 열린 노드의 스테이징 크기와 시각을 반영합니다.
    pub fn list(&self, path: &RemotePath) -> Result<Arc<Vec<Entry>>> {
        let list = match self.meta.dir(path) {
            Some(l) => l,
            None => {
                let l = Arc::new(self.run(self.backend.list(path))?);
                self.meta.put_dir(path, l.clone());
                l
            }
        };
        let overlay: Vec<(String, Entry)> = {
            let nodes = self.nodes.lock();
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
        let mut nodes = self.nodes.lock();
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
                        pending_mtime: None,
                        deleted: false,
                    }),
                })
            })
            .clone();
        node.opens.fetch_add(1, Ordering::SeqCst);
        Arc::new(FileHandle { node })
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
        let entry = match kind {
            EntryKind::Dir => self.run(self.backend.mkdir(path))?,
            EntryKind::File => {
                self.run(self.backend.write(path, WriteSource::Bytes(Bytes::new())))?
            }
        };
        self.meta.invalidate(path);
        self.meta.put_entry(path, entry.clone());
        Ok(self.attach(path.clone(), entry))
    }

    pub fn info(&self, h: &FileHandle) -> Entry {
        h.node.state.lock().entry.clone()
    }

    pub fn read(&self, h: &FileHandle, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let size = {
            let st = h.node.state.lock();
            if let Some(staged) = &st.staged {
                return read_staged(staged, st.entry.size, offset, buf);
            }
            st.entry.size
        };
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let len = (buf.len() as u64).min(size - offset);
        let path = h.path();
        let data = self.run(self.backend.read_range(&path, offset, len))?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    /// 스테이징 파일이 없으면 만들고 현재 원격 내용을 채웁니다.
    fn ensure_staged(&self, path: &RemotePath, st: &mut NodeState) -> Result<()> {
        if st.staged.is_some() {
            return Ok(());
        }
        let mut tmp = NamedTempFile::new_in(&self.opts.staging_dir)?;
        let size = st.entry.size;
        let mut off = 0;
        while off < size {
            let chunk = self.run(self.backend.read_range(
                path,
                off,
                DOWNLOAD_CHUNK.min(size - off),
            ))?;
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
        let path = h.path();
        if new_size == 0 {
            st.staged = Some(NamedTempFile::new_in(&self.opts.staging_dir)?);
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
        let mut st = h.node.state.lock();
        if st.dirty {
            st.pending_mtime = Some(mtime);
            st.entry.modified = Some(mtime);
        } else if self.backend.capabilities().set_mtime && !self.opts.read_only {
            self.run(self.backend.set_mtime(&h.path(), mtime))?;
            st.entry.modified = Some(mtime);
            self.meta.invalidate(&h.path());
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
        let from = h.path();
        if from.is_root() || to.is_root() {
            return Err(Error::PermissionDenied);
        }
        if h.is_dir() && to.starts_with(&from) && !to.as_str().eq_ignore_ascii_case(from.as_str()) {
            return Err(Error::InvalidPath(
                "디렉터리를 자기 하위로 옮길 수 없습니다".into(),
            ));
        }
        if h.is_dir() && !self.backend.capabilities().rename_dir {
            return Err(Error::Unsupported("디렉터리 이름 변경"));
        }
        self.run(self.backend.rename(&from, to, replace))?;

        let mut nodes = self.nodes.lock();
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
        self.meta.invalidate_tree(&from);
        self.meta.invalidate_tree(to);
        Ok(())
    }

    /// 핸들 정리. `delete`면 원격에서 삭제하고, 마지막 핸들이면 변경분을 업로드합니다.
    pub fn cleanup(&self, h: &FileHandle, delete: bool) -> Result<()> {
        let remaining = h.node.opens.fetch_sub(1, Ordering::SeqCst) - 1;
        let path = h.path();
        let result = if delete {
            self.delete_node(h, &path)
        } else if remaining == 0 {
            self.upload_if_dirty(h)
        } else {
            Ok(())
        };
        if remaining == 0 {
            let mut nodes = self.nodes.lock();
            if nodes.get(&path).is_some_and(|n| Arc::ptr_eq(n, &h.node)) {
                nodes.remove(&path);
            }
        }
        result
    }

    /// 핸들 해제. Cleanup 뒤 페이징 쓰기로 다시 변경된 내용이 있으면 업로드합니다.
    pub fn close(&self, h: &FileHandle) {
        if let Err(e) = self.upload_if_dirty(h) {
            tracing::error!(path = %h.path(), error = %e, "닫기 시 업로드 실패");
        }
    }

    pub fn flush(&self, h: &FileHandle) -> Result<Entry> {
        Ok(self.info(h))
    }

    fn delete_node(&self, h: &FileHandle, path: &RemotePath) -> Result<()> {
        self.writable()?;
        let mut st = h.node.state.lock();
        self.run(self.backend.delete(path, h.node.kind))?;
        st.staged = None;
        st.dirty = false;
        st.deleted = true;
        self.meta.invalidate_tree(path);
        Ok(())
    }

    fn upload_if_dirty(&self, h: &FileHandle) -> Result<()> {
        let mut st = h.node.state.lock();
        if !st.dirty || st.deleted {
            return Ok(());
        }
        let path = h.path();
        let staged = st.staged.as_ref().expect("dirty node has staging");
        let source = WriteSource::File {
            path: staged.path().to_owned(),
            size: st.entry.size,
        };
        let mut entry = self.run(self.backend.write(&path, source))?;
        if let Some(mtime) = st.pending_mtime.take() {
            if self.backend.capabilities().set_mtime {
                self.run(self.backend.set_mtime(&path, mtime))?;
                entry.modified = Some(mtime);
            }
        }
        tracing::debug!(%path, size = entry.size, "업로드 완료");
        st.entry = entry.clone();
        st.dirty = false;
        st.staged = None;
        self.meta.invalidate(&path);
        self.meta.put_entry(&path, entry);
        Ok(())
    }

    pub fn quota(&self) -> Quota {
        let mut q = self.quota.lock();
        if let Some((v, at)) = *q {
            if at.elapsed() < QUOTA_TTL {
                return v;
            }
        }
        let v = self.run(self.backend.quota()).unwrap_or_default();
        *q = Some((v, Instant::now()));
        v
    }

    /// 원격 변경을 다시 읽도록 메타 캐시를 비웁니다.
    pub fn refresh(&self) {
        self.meta.clear();
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
