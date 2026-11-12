use super::*;
use expandrv_backends::local::LocalBackend;
use std::sync::atomic::{AtomicBool, AtomicU32};

#[path = "performance_tests.rs"]
mod performance;

/// 지연, 오프라인, 실패와 호출 횟수를 흉내 내는 시험용 백엔드
struct TestBackend {
    inner: LocalBackend,
    write_delay: Mutex<Duration>,
    metadata_delay: Mutex<Duration>,
    metadata_calls: AtomicUsize,
    offline: AtomicBool,
    fail_writes: AtomicU32,
    /// 다음 읽기 몇 번을 일시 오류로 실패시킵니다.
    fail_reads: AtomicU32,
    /// 성공한 읽기가 이 횟수에 이르면 이후 읽기를 모두 일시 오류로 실패시킵니다.
    fail_reads_after: AtomicUsize,
    reads: AtomicUsize,
    /// 성공한 읽기로 받은 바이트 합계
    read_bytes: AtomicUsize,
    /// 연속 전송 백엔드처럼 동작합니다.
    sequential: AtomicBool,
    /// 읽기 요청 위치 순서와 동시에 진행된 읽기 수
    read_offsets: Mutex<Vec<u64>>,
    reading: AtomicUsize,
    max_reading: AtomicUsize,
    writes: AtomicUsize,
    read_delay: Mutex<Duration>,
    started_writes: AtomicUsize,
    connections: Semaphore,
}

impl TestBackend {
    fn check(&self) -> Result<()> {
        if self.offline.load(Ordering::SeqCst) {
            Err(Error::Network("offline".into()))
        } else {
            Ok(())
        }
    }
}

#[async_trait::async_trait]
impl Backend for TestBackend {
    fn kind(&self) -> &'static str {
        "test"
    }
    fn capabilities(&self) -> expandrv_core::Capabilities {
        expandrv_core::Capabilities {
            sequential_transfer: self.sequential.load(Ordering::SeqCst),
            ..self.inner.capabilities()
        }
    }
    async fn stat(&self, path: &RemotePath) -> Result<Entry> {
        let _connection = self.connections.acquire().await.unwrap();
        self.metadata_calls.fetch_add(1, Ordering::SeqCst);
        let delay = *self.metadata_delay.lock();
        tokio::time::sleep(delay).await;
        self.check()?;
        self.inner.stat(path).await
    }
    async fn list(&self, path: &RemotePath) -> Result<Vec<Entry>> {
        let _connection = self.connections.acquire().await.unwrap();
        self.metadata_calls.fetch_add(1, Ordering::SeqCst);
        let delay = *self.metadata_delay.lock();
        tokio::time::sleep(delay).await;
        self.check()?;
        self.inner.list(path).await
    }
    async fn read_range(&self, path: &RemotePath, offset: u64, len: u64) -> Result<Bytes> {
        let _connection = self.connections.acquire().await.unwrap();
        self.read_offsets.lock().push(offset);
        let now = self.reading.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_reading.fetch_max(now, Ordering::SeqCst);
        struct Done<'a>(&'a AtomicUsize);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let _done = Done(&self.reading);
        let delay = *self.read_delay.lock();
        tokio::time::sleep(delay).await;
        self.check()?;
        if self
            .fail_reads
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
            || self.reads.load(Ordering::SeqCst) >= self.fail_reads_after.load(Ordering::SeqCst)
        {
            return Err(Error::Network("injected read".into()));
        }
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.read_bytes.fetch_add(len as usize, Ordering::SeqCst);
        self.inner.read_range(path, offset, len).await
    }
    async fn write(&self, path: &RemotePath, source: WriteSource) -> Result<Entry> {
        let _connection = self.connections.acquire().await.unwrap();
        self.started_writes.fetch_add(1, Ordering::SeqCst);
        self.check()?;
        let delay = *self.write_delay.lock();
        tokio::time::sleep(delay).await;
        if self
            .fail_writes
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(Error::Network("injected".into()));
        }
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.write(path, source).await
    }
    async fn mkdir(&self, path: &RemotePath) -> Result<Entry> {
        self.inner.mkdir(path).await
    }
    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> Result<()> {
        self.inner.rename(from, to, replace).await
    }
    async fn delete(&self, path: &RemotePath, kind: EntryKind) -> Result<()> {
        self.inner.delete(path, kind).await
    }
}

struct Fixture {
    _rt: tokio::runtime::Runtime,
    root: tempfile::TempDir,
    staging: tempfile::TempDir,
    backend: Arc<TestBackend>,
    vfs: Vfs,
    opts: VfsOptions,
}

fn fixture_with(tune: impl FnOnce(&mut VfsOptions)) -> Fixture {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let backend = Arc::new(TestBackend {
        inner: LocalBackend::new(root.path()).unwrap(),
        write_delay: Mutex::new(Duration::ZERO),
        metadata_delay: Mutex::new(Duration::ZERO),
        metadata_calls: AtomicUsize::new(0),
        offline: AtomicBool::new(false),
        fail_writes: AtomicU32::new(0),
        reads: AtomicUsize::new(0),
        read_bytes: AtomicUsize::new(0),
        sequential: AtomicBool::new(false),
        read_offsets: Mutex::new(Vec::new()),
        reading: AtomicUsize::new(0),
        max_reading: AtomicUsize::new(0),
        fail_reads: AtomicU32::new(0),
        fail_reads_after: AtomicUsize::new(usize::MAX),
        writes: AtomicUsize::new(0),
        read_delay: Mutex::new(Duration::ZERO),
        started_writes: AtomicUsize::new(0),
        connections: Semaphore::new(4),
    });
    let mut opts = VfsOptions::new(staging.path().join("stage"));
    opts.metadata_ttl = Duration::from_secs(60);
    opts.op_timeout = Duration::from_secs(10);
    tune(&mut opts);
    let vfs = Vfs::new(backend.clone(), rt.handle().clone(), opts.clone()).unwrap();
    Fixture {
        _rt: rt,
        root,
        staging,
        backend,
        vfs,
        opts,
    }
}

fn fixture(read_only: bool) -> Fixture {
    fixture_with(|o| o.read_only = read_only)
}

fn p(s: &str) -> RemotePath {
    RemotePath::parse(s).unwrap()
}

fn staging_files(f: &Fixture) -> usize {
    std::fs::read_dir(f.staging.path().join("stage"))
        .unwrap()
        .count()
}

fn sync(f: &Fixture) {
    f.vfs.sync(Duration::from_secs(15)).unwrap();
}

#[test]
fn create_write_upload_after_last_cleanup() {
    let f = fixture(false);
    let h = f.vfs.create(&p("/a.txt"), EntryKind::File).unwrap();
    let (n, e) = f.vfs.write(&h, 0, b"hello", false, false).unwrap();
    assert_eq!((n, e.size), (5, 5));
    f.vfs.write(&h, 0, b" world", true, false).unwrap();

    // 업로드 전에는 원격이 비어 있고, 목록과 속성은 스테이징 크기를 보여 줍니다.
    assert_eq!(std::fs::read(f.root.path().join("a.txt")).unwrap(), b"");
    assert_eq!(f.vfs.stat(&p("/a.txt")).unwrap().1.size, 11);
    assert_eq!(f.vfs.list(&RemotePath::root()).unwrap()[0].size, 11);
    assert_eq!(f.vfs.pending_uploads(), 1);

    f.vfs.cleanup(&h, false).unwrap();
    f.vfs.close(&h);
    sync(&f);
    assert_eq!(
        std::fs::read(f.root.path().join("a.txt")).unwrap(),
        b"hello world"
    );
    assert_eq!(f.vfs.pending_uploads(), 0);
    assert_eq!(staging_files(&f), 0);
    assert_eq!(
        f.vfs.take_transfer_events(),
        vec![TransferEvent::Uploaded(p("/a.txt"))]
    );
    assert!(f.vfs.take_transfer_events().is_empty());
}

#[test]
fn partial_write_preserves_remote_content() {
    let f = fixture(false);
    std::fs::write(f.root.path().join("b.bin"), b"0123456789").unwrap();
    let h = f.vfs.open(&p("/b.bin")).unwrap();
    let mut buf = [0u8; 4];
    assert_eq!(f.vfs.read(&h, 3, &mut buf).unwrap(), 4);
    assert_eq!(&buf, b"3456");

    f.vfs.write(&h, 2, b"AB", false, false).unwrap();
    // 페이징 쓰기는 파일을 늘리지 않습니다.
    assert_eq!(f.vfs.write(&h, 9, b"XYZ", false, true).unwrap().0, 1);
    f.vfs.cleanup(&h, false).unwrap();
    sync(&f);
    assert_eq!(
        std::fs::read(f.root.path().join("b.bin")).unwrap(),
        b"01AB45678X"
    );
}

#[test]
fn truncate_and_extend() {
    let f = fixture(false);
    std::fs::write(f.root.path().join("c"), b"abcdef").unwrap();
    let h = f.vfs.open(&p("/c")).unwrap();
    assert_eq!(f.vfs.set_size(&h, 3, false).unwrap().size, 3);
    assert_eq!(f.vfs.set_size(&h, 10, true).unwrap().size, 3); // 할당 확장은 크기 불변
    assert_eq!(f.vfs.set_size(&h, 5, false).unwrap().size, 5);
    f.vfs.cleanup(&h, false).unwrap();
    sync(&f);
    assert_eq!(std::fs::read(f.root.path().join("c")).unwrap(), b"abc\0\0");

    let h = f.vfs.open(&p("/c")).unwrap();
    f.vfs.overwrite(&h).unwrap();
    f.vfs.cleanup(&h, false).unwrap();
    sync(&f);
    assert_eq!(std::fs::metadata(f.root.path().join("c")).unwrap().len(), 0);
}

#[test]
fn rename_open_file_then_upload_to_new_path() {
    let f = fixture(false);
    std::fs::create_dir(f.root.path().join("d")).unwrap();
    let h = f.vfs.create(&p("/d/x.txt"), EntryKind::File).unwrap();
    f.vfs.write(&h, 0, b"data", false, false).unwrap();
    f.vfs.rename(&h, &p("/d/y.txt"), false).unwrap();
    assert_eq!(h.path().as_str(), "/d/y.txt");
    f.vfs.cleanup(&h, false).unwrap();
    sync(&f);
    assert_eq!(
        std::fs::read(f.root.path().join("d/y.txt")).unwrap(),
        b"data"
    );
    assert!(matches!(f.vfs.stat(&p("/d/x.txt")), Err(Error::NotFound)));

    let dh = f.vfs.open(&p("/d")).unwrap();
    f.vfs.rename(&dh, &p("/e"), false).unwrap();
    f.vfs.cleanup(&dh, false).unwrap();
    assert_eq!(f.vfs.stat(&p("/e/y.txt")).unwrap().1.size, 4);
}

#[test]
fn rename_during_upload_uploads_to_new_path() {
    let f = fixture(false);
    *f.backend.write_delay.lock() = Duration::from_millis(300);
    let h = f.vfs.create(&p("/old.txt"), EntryKind::File).unwrap();
    f.vfs.write(&h, 0, b"payload", false, false).unwrap();
    f.vfs.cleanup(&h, false).unwrap(); // 업로드 예약(지연 중)
    let h2 = f.vfs.open(&p("/old.txt")).unwrap();
    f.vfs.rename(&h2, &p("/new.txt"), false).unwrap();
    f.vfs.cleanup(&h2, false).unwrap();
    sync(&f);
    assert_eq!(
        std::fs::read(f.root.path().join("new.txt")).unwrap(),
        b"payload"
    );
    assert!(!f.root.path().join("old.txt").exists());
}

#[test]
fn rewrite_cancels_inflight_upload() {
    let f = fixture(false);
    *f.backend.write_delay.lock() = Duration::from_millis(400);
    let h = f.vfs.create(&p("/doc"), EntryKind::File).unwrap();
    let creates = f.backend.writes.load(Ordering::SeqCst);
    f.vfs.write(&h, 0, b"version-1", false, false).unwrap();
    f.vfs.cleanup(&h, false).unwrap();

    // 업로드가 끝나기 전에 다시 열어 바꾸면 스테이징 내용을 읽고 첫 업로드는 취소됩니다.
    let h = f.vfs.open(&p("/doc")).unwrap();
    let mut buf = [0u8; 9];
    f.vfs.read(&h, 0, &mut buf).unwrap();
    assert_eq!(&buf, b"version-1");
    f.vfs.write(&h, 8, b"2", false, false).unwrap();
    f.vfs.cleanup(&h, false).unwrap();
    sync(&f);
    assert_eq!(
        std::fs::read(f.root.path().join("doc")).unwrap(),
        b"version-2"
    );
    assert_eq!(f.backend.writes.load(Ordering::SeqCst) - creates, 1);
}

#[test]
fn failed_upload_is_retried() {
    let f = fixture(false);
    f.backend.fail_writes.store(1, Ordering::SeqCst);
    std::fs::write(f.root.path().join("r.txt"), b"old").unwrap();
    let h = f.vfs.open(&p("/r.txt")).unwrap();
    f.vfs.overwrite(&h).unwrap();
    f.vfs.write(&h, 0, b"new", false, false).unwrap();
    f.vfs.cleanup(&h, false).unwrap();
    sync(&f);
    assert_eq!(std::fs::read(f.root.path().join("r.txt")).unwrap(), b"new");
}

#[test]
fn block_cache_serves_repeated_and_sequential_reads() {
    let f = fixture_with(|o| {
        o.block_size = 64 * 1024;
        o.readahead_blocks = 0;
    });
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(f.root.path().join("big"), &data).unwrap();
    let h = f.vfs.open(&p("/big")).unwrap();
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = f.vfs.read(&h, out.len() as u64, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    assert_eq!(out, data);
    let reads = f.backend.reads.load(Ordering::SeqCst);
    assert_eq!(reads, 4, "64KiB 블록 4개");
    f.vfs.read(&h, 70_000, &mut buf).unwrap();
    assert_eq!(f.backend.reads.load(Ordering::SeqCst), reads);
}

#[test]
fn offline_fallback_uses_saved_listing() {
    let f = fixture_with(|o| o.meta_db = Some(o.staging_dir.with_file_name("meta.db")));
    std::fs::create_dir(f.root.path().join("Docs")).unwrap();
    std::fs::write(f.root.path().join("Docs/a.txt"), b"1").unwrap();
    f.vfs.list(&RemotePath::root()).unwrap();
    f.vfs.list(&p("/Docs")).unwrap();

    f.backend.offline.store(true, Ordering::SeqCst);
    f.vfs.refresh();
    assert_eq!(f.vfs.list(&p("/Docs")).unwrap()[0].name, "a.txt");
    assert_eq!(f.vfs.stat(&p("/docs")).unwrap().0.as_str(), "/Docs");
    assert!(matches!(
        f.vfs.list(&p("/Docs/none")),
        Err(Error::Network(_))
    ));
}

#[test]
fn delete_rules() {
    let f = fixture(false);
    let d = f.vfs.create(&p("/dir"), EntryKind::Dir).unwrap();
    let file = f.vfs.create(&p("/dir/f"), EntryKind::File).unwrap();
    assert!(matches!(
        f.vfs.check_delete(&d),
        Err(Error::DirectoryNotEmpty)
    ));
    f.vfs.check_delete(&file).unwrap();
    f.vfs.cleanup(&file, true).unwrap();
    f.vfs.check_delete(&d).unwrap();
    f.vfs.cleanup(&d, true).unwrap();
    assert!(!f.root.path().join("dir").exists());
    assert!(matches!(f.vfs.stat(&p("/dir")), Err(Error::NotFound)));
}

#[test]
fn delete_while_dirty_discards_upload() {
    let f = fixture(false);
    let h = f.vfs.create(&p("/tmp.txt"), EntryKind::File).unwrap();
    f.vfs.write(&h, 0, b"scratch", false, false).unwrap();
    f.vfs.check_delete(&h).unwrap();
    f.vfs.cleanup(&h, true).unwrap();
    sync(&f);
    assert!(!f.root.path().join("tmp.txt").exists());
    assert_eq!(staging_files(&f), 0);
}

#[test]
fn cached_listing_answers_missing_names_without_backend() {
    let f = fixture(false);
    std::fs::write(f.root.path().join("Real.txt"), b"1").unwrap();
    f.vfs.list(&RemotePath::root()).unwrap();
    // 목록 캐시가 있으면 원격에 없는 이름은 바로 NotFound입니다.
    std::fs::write(f.root.path().join("late.txt"), b"1").unwrap();
    assert!(matches!(f.vfs.stat(&p("/late.txt")), Err(Error::NotFound)));
    assert_eq!(f.vfs.stat(&p("/real.TXT")).unwrap().0.as_str(), "/Real.txt");
    f.vfs.refresh();
    assert!(f.vfs.stat(&p("/late.txt")).is_ok());
}

#[test]
fn read_only_rejects_mutations() {
    let f = fixture(true);
    std::fs::write(f.root.path().join("r"), b"x").unwrap();
    assert!(matches!(
        f.vfs.create(&p("/n"), EntryKind::File),
        Err(Error::ReadOnly)
    ));
    let h = f.vfs.open(&p("/r")).unwrap();
    assert!(matches!(
        f.vfs.write(&h, 0, b"y", false, false),
        Err(Error::ReadOnly)
    ));
    assert!(matches!(
        f.vfs.rename(&h, &p("/s"), false),
        Err(Error::ReadOnly)
    ));
    assert!(matches!(f.vfs.check_delete(&h), Err(Error::ReadOnly)));
}

#[test]
fn create_existing_fails() {
    let f = fixture(false);
    std::fs::write(f.root.path().join("e"), b"x").unwrap();
    assert!(matches!(
        f.vfs.create(&p("/e"), EntryKind::File),
        Err(Error::AlreadyExists)
    ));
    assert!(matches!(
        f.vfs.create(&p("/e/x"), EntryKind::File),
        Err(Error::NotADirectory)
    ));
}

#[test]
fn poll_detects_remote_changes_in_watched_dirs() {
    let f = fixture(false);
    std::fs::write(f.root.path().join("keep.txt"), b"1").unwrap();
    std::fs::write(f.root.path().join("edit.txt"), b"1").unwrap();
    std::fs::write(f.root.path().join("gone.txt"), b"1").unwrap();
    f.vfs.list(&RemotePath::root()).unwrap();
    assert!(f.vfs.poll_changes().is_empty());

    std::fs::write(f.root.path().join("new.txt"), b"1").unwrap();
    std::fs::write(f.root.path().join("edit.txt"), b"12345").unwrap();
    std::fs::remove_file(f.root.path().join("gone.txt")).unwrap();
    std::fs::create_dir(f.root.path().join("dir")).unwrap();
    let got: Vec<(String, ChangeKind)> = f
        .vfs
        .poll_changes()
        .into_iter()
        .map(|c| (c.path.to_string(), c.kind))
        .collect();
    assert_eq!(
        got,
        [
            ("/dir".to_owned(), ChangeKind::Added),
            ("/edit.txt".to_owned(), ChangeKind::Modified),
            ("/gone.txt".to_owned(), ChangeKind::Removed),
            ("/new.txt".to_owned(), ChangeKind::Added),
        ]
    );
    // 캐시가 새 목록으로 바뀌어 바로 보입니다.
    assert_eq!(f.vfs.stat(&p("/edit.txt")).unwrap().1.size, 5);
    assert!(matches!(f.vfs.stat(&p("/gone.txt")), Err(Error::NotFound)));
    assert!(f.vfs.poll_changes().is_empty());
}

#[test]
fn local_changes_are_not_reported_as_remote() {
    let f = fixture(false);
    f.vfs.list(&RemotePath::root()).unwrap();
    let h = f.vfs.create(&p("/mine.txt"), EntryKind::File).unwrap();
    f.vfs.write(&h, 0, b"abc", false, false).unwrap();
    f.vfs.cleanup(&h, false).unwrap();
    sync(&f);
    assert!(f.vfs.poll_changes().is_empty());
}

#[test]
fn throttle_wraps_only_when_limited() {
    let f = fixture(false);
    let be: Arc<dyn Backend> = f.backend.clone();
    assert!(Arc::ptr_eq(&throttle(be.clone(), 0, 0), &be));
    let t = throttle(be.clone(), 1 << 20, 0);
    assert!(!Arc::ptr_eq(&t, &be));
    assert_eq!(t.kind(), "test");
}

/// 업로드 저널을 쓰는 픽스처(메타 DB 사용)
fn journaled() -> Fixture {
    fixture_with(|o| o.meta_db = Some(o.staging_dir.with_file_name("meta.db")))
}

/// 업로드가 계속 실패하는 상태에서 파일을 바꾸고 정리까지 마칩니다.
fn edit_with_failing_upload(f: &Fixture, open: &str, rename_to: Option<&str>, body: &[u8]) {
    f.backend.fail_writes.store(1_000, Ordering::SeqCst);
    let h = f.vfs.open(&p(open)).unwrap();
    f.vfs.overwrite(&h).unwrap();
    f.vfs.write(&h, 0, body, false, false).unwrap();
    if let Some(to) = rename_to {
        f.vfs.rename(&h, &p(to), false).unwrap();
    }
    f.vfs.cleanup(&h, false).unwrap();
    f.vfs.close(&h);
    assert!(f.vfs.sync(Duration::from_millis(300)).is_err());
}

/// 기존 VFS를 끝낸 것처럼 정리하고 같은 설정으로 다시 엽니다. `crash`면 정리 코드를 돌리지 않습니다.
fn restart(f: Fixture, crash: bool) -> (Fixture, Vfs) {
    f.vfs.shutdown();
    let old = f.vfs.clone();
    if crash {
        std::mem::forget(old);
    } else {
        drop(old);
    }
    f.backend.fail_writes.store(0, Ordering::SeqCst);
    let vfs = Vfs::new(f.backend.clone(), f._rt.handle().clone(), f.opts.clone()).unwrap();
    (f, vfs)
}

#[test]
fn pending_upload_resumes_after_failed_shutdown() {
    let f = journaled();
    std::fs::write(f.root.path().join("doc.txt"), b"old").unwrap();
    edit_with_failing_upload(&f, "/doc.txt", None, b"new content");
    f.vfs.shutdown();
    let Fixture {
        _rt: rt,
        root,
        staging,
        backend,
        vfs,
        opts,
    } = f;
    // 모든 핸들을 놓고 중단된 업로드 작업이 정리될 때까지 기다리면 노드 상태가 해제됩니다.
    drop(vfs);
    std::thread::sleep(Duration::from_millis(300));
    let left = std::fs::read_dir(staging.path().join("stage"))
        .unwrap()
        .count();
    assert_eq!(left, 1, "올리지 못한 스테이징 파일이 지워졌습니다");

    backend.fail_writes.store(0, Ordering::SeqCst);
    let vfs = Vfs::new(backend.clone(), rt.handle().clone(), opts).unwrap();
    vfs.sync(Duration::from_secs(10)).unwrap();
    assert_eq!(
        std::fs::read(root.path().join("doc.txt")).unwrap(),
        b"new content"
    );
}

#[test]
fn pending_upload_resumes_after_crash_and_cleans_staging() {
    let f = journaled();
    std::fs::write(f.root.path().join("crash.bin"), b"old").unwrap();
    edit_with_failing_upload(&f, "/crash.bin", None, b"survived");
    assert_eq!(staging_files(&f), 1);
    let (f, vfs) = restart(f, true);
    vfs.sync(Duration::from_secs(10)).unwrap();
    assert_eq!(
        std::fs::read(f.root.path().join("crash.bin")).unwrap(),
        b"survived"
    );
    assert_eq!(staging_files(&f), 0, "업로드 뒤 스테이징 파일이 남았습니다");
    // 저널이 비었으므로 다시 열어도 올릴 것이 없습니다.
    let again = Vfs::new(f.backend.clone(), f._rt.handle().clone(), f.opts.clone()).unwrap();
    assert_eq!(again.pending_uploads(), 0);
}

#[test]
fn recovered_upload_keeps_remote_change_and_writes_conflict_copy() {
    let f = journaled();
    std::fs::write(f.root.path().join("report.txt"), b"base").unwrap();
    edit_with_failing_upload(&f, "/report.txt", None, b"local edit");
    // 재시작 전에 다른 곳에서 원격 파일이 바뀝니다.
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(f.root.path().join("report.txt"), b"remote edit by someone").unwrap();
    let (f, vfs) = restart(f, true);
    vfs.sync(Duration::from_secs(10)).unwrap();
    assert_eq!(
        std::fs::read(f.root.path().join("report.txt")).unwrap(),
        b"remote edit by someone"
    );
    let copies: Vec<String> = std::fs::read_dir(f.root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n.starts_with("report (ExpanDrv 충돌 ") && n.ends_with(").txt"))
        .collect();
    assert_eq!(copies.len(), 1, "{copies:?}");
    assert_eq!(
        vfs.take_transfer_events(),
        vec![TransferEvent::Conflict(p(&format!("/{}", copies[0])))]
    );
    assert_eq!(
        std::fs::read(f.root.path().join(&copies[0])).unwrap(),
        b"local edit"
    );
}

#[test]
fn journal_follows_rename_before_restart() {
    let f = journaled();
    std::fs::write(f.root.path().join("a.txt"), b"old").unwrap();
    edit_with_failing_upload(&f, "/a.txt", Some("/b.txt"), b"renamed edit");
    let (f, vfs) = restart(f, true);
    vfs.sync(Duration::from_secs(10)).unwrap();
    assert_eq!(
        std::fs::read(f.root.path().join("b.txt")).unwrap(),
        b"renamed edit"
    );
    assert!(!f.root.path().join("a.txt").exists());
}

#[test]
fn conflict_name_format() {
    let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    assert_eq!(
        conflict_path(&p("/d/report.final.docx"), t)
            .unwrap()
            .as_str(),
        "/d/report.final (ExpanDrv 충돌 2023-11-14 221320).docx"
    );
    assert_eq!(
        conflict_path(&p("/.bashrc"), t).unwrap().as_str(),
        "/.bashrc (ExpanDrv 충돌 2023-11-14 221320)"
    );
    assert_eq!(
        conflict_path(&p("/noext"), UNIX_EPOCH).unwrap().as_str(),
        "/noext (ExpanDrv 충돌 1970-01-01 000000)"
    );
}

#[test]
fn transient_read_errors_are_retried() {
    let f = fixture_with(|o| o.block_size = 64 * 1024);
    std::fs::write(f.root.path().join("r.bin"), vec![5u8; 1000]).unwrap();
    let h = f.vfs.open(&p("/r.bin")).unwrap();
    f.backend.fail_reads.store(2, Ordering::SeqCst);
    let mut buf = [0u8; 1000];
    assert_eq!(f.vfs.read(&h, 0, &mut buf).unwrap(), 1000);
    assert_eq!(buf[999], 5);

    // 재시도 횟수를 넘는 일시 오류는 그대로 돌려줍니다.
    let h2 = {
        std::fs::write(f.root.path().join("r2.bin"), vec![1u8; 10]).unwrap();
        f.vfs.open(&p("/r2.bin")).unwrap()
    };
    f.backend.fail_reads.store(10, Ordering::SeqCst);
    assert!(matches!(
        f.vfs.read(&h2, 0, &mut buf),
        Err(Error::Network(_))
    ));
    f.backend.fail_reads.store(0, Ordering::SeqCst);
}

#[test]
fn interrupted_staging_download_resumes_without_refetch() {
    let f = fixture_with(|o| {
        o.block_size = 64 * 1024;
        o.readahead_blocks = 0;
    });
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 249) as u8).collect();
    std::fs::write(f.root.path().join("big.bin"), &data).unwrap();
    let h = f.vfs.open(&p("/big.bin")).unwrap();

    // 블록 2개를 받은 뒤 끊깁니다.
    f.backend.fail_reads_after.store(2, Ordering::SeqCst);
    assert!(f.vfs.write(&h, 0, b"X", false, false).is_err());
    assert_eq!(f.backend.reads.load(Ordering::SeqCst), 2);

    // 연결이 돌아오고 대기 시간이 지나면 나머지 3블록만 받아 마칩니다.
    *f.vfs.s.offline_until.lock() = Some(Instant::now() - Duration::from_secs(1));
    f.backend
        .fail_reads_after
        .store(usize::MAX, Ordering::SeqCst);
    f.vfs.write(&h, 0, b"X", false, false).unwrap();
    assert_eq!(
        f.backend.reads.load(Ordering::SeqCst),
        5,
        "이미 받은 블록을 다시 받았습니다"
    );
    f.vfs.cleanup(&h, false).unwrap();
    sync(&f);
    let mut want = data.clone();
    want[0] = b'X';
    assert_eq!(std::fs::read(f.root.path().join("big.bin")).unwrap(), want);
}

fn with_disk_cache(o: &mut VfsOptions, key: [u8; 32]) {
    o.block_size = 64 * 1024;
    o.readahead_blocks = 0;
    o.meta_db = Some(o.staging_dir.with_file_name("meta.db"));
    o.disk_cache = Some(DiskCacheOptions {
        dir: o.staging_dir.with_file_name("blocks"),
        max_bytes: 16 << 20,
        key,
    });
}

/// 디스크 캐시 저장은 백그라운드로 진행되므로 블록 파일이 생길 때까지 기다립니다.
fn wait_disk_blocks(f: &Fixture, n: usize) {
    let dir = f.staging.path().join("blocks");
    for _ in 0..100 {
        let count: usize = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().is_dir())
                    .map(|e| std::fs::read_dir(e.path()).map_or(0, |r| r.count()))
                    .sum()
            })
            .unwrap_or(0);
        if count >= n {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("디스크 캐시 블록이 {n}개 생기지 않았습니다");
}

#[test]
fn disk_cache_serves_reads_after_restart_and_offline() {
    let key = generate_cache_key().unwrap();
    let f = fixture_with(|o| with_disk_cache(o, key));
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 241) as u8).collect();
    std::fs::create_dir(f.root.path().join("docs")).unwrap();
    std::fs::write(f.root.path().join("docs/report.bin"), &data).unwrap();
    f.vfs.list(&RemotePath::root()).unwrap();
    f.vfs.list(&p("/docs")).unwrap();
    let h = f.vfs.open(&p("/docs/report.bin")).unwrap();
    let mut buf = vec![0u8; data.len()];
    assert_eq!(f.vfs.read(&h, 0, &mut buf).unwrap(), data.len());
    f.vfs.cleanup(&h, false).unwrap();
    wait_disk_blocks(&f, 4);

    // 다시 시작했고 원격은 응답하지 않습니다.
    f.backend.offline.store(true, Ordering::SeqCst);
    let reads_before = f.backend.reads.load(Ordering::SeqCst);
    let vfs = Vfs::new(f.backend.clone(), f._rt.handle().clone(), f.opts.clone()).unwrap();
    let h = vfs.open(&p("/docs/report.bin")).unwrap();
    let mut again = vec![0u8; data.len()];
    assert_eq!(vfs.read(&h, 0, &mut again).unwrap(), data.len());
    assert_eq!(again, data);
    assert_eq!(f.backend.reads.load(Ordering::SeqCst), reads_before);
    assert_eq!(vfs.active_downloads(), 0);
}

#[test]
fn disk_cache_is_invalidated_by_upload() {
    let key = generate_cache_key().unwrap();
    let f = fixture_with(|o| with_disk_cache(o, key));
    std::fs::write(f.root.path().join("v.txt"), b"version-1").unwrap();
    let h = f.vfs.open(&p("/v.txt")).unwrap();
    let mut buf = [0u8; 9];
    f.vfs.read(&h, 0, &mut buf).unwrap();
    wait_disk_blocks(&f, 1);
    f.vfs.write(&h, 8, b"2", false, false).unwrap();
    f.vfs.cleanup(&h, false).unwrap();
    sync(&f);

    let vfs = Vfs::new(f.backend.clone(), f._rt.handle().clone(), f.opts.clone()).unwrap();
    let h = vfs.open(&p("/v.txt")).unwrap();
    vfs.read(&h, 0, &mut buf).unwrap();
    assert_eq!(&buf, b"version-2");
}

#[test]
fn offline_open_by_path_uses_saved_entry_and_disk_cache() {
    let key = generate_cache_key().unwrap();
    let f = fixture_with(|o| with_disk_cache(o, key));
    let data: Vec<u8> = (0..100_000u32).map(|i| (i % 239) as u8).collect();
    std::fs::write(f.root.path().join("direct.bin"), &data).unwrap();
    // 폴더 목록 없이 경로로 바로 엽니다.
    let h = f.vfs.open(&p("/direct.bin")).unwrap();
    let mut buf = vec![0u8; data.len()];
    f.vfs.read(&h, 0, &mut buf).unwrap();
    f.vfs.cleanup(&h, false).unwrap();
    wait_disk_blocks(&f, 2);

    f.backend.offline.store(true, Ordering::SeqCst);
    let vfs = Vfs::new(f.backend.clone(), f._rt.handle().clone(), f.opts.clone()).unwrap();
    let h = vfs.open(&p("/direct.bin")).unwrap();
    let mut again = vec![0u8; data.len()];
    assert_eq!(vfs.read(&h, 0, &mut again).unwrap(), data.len());
    assert_eq!(again, data);
}

#[test]
fn offline_cooldown_skips_remote_and_recovers_after_deadline() {
    let f = fixture_with(|o| {
        o.meta_db = Some(o.staging_dir.with_file_name("meta.db"));
        o.op_timeout = Duration::from_millis(50);
    });
    std::fs::write(f.root.path().join("cached.txt"), b"old").unwrap();
    f.vfs.list(&RemotePath::root()).unwrap();
    f.vfs.refresh();
    *f.backend.metadata_delay.lock() = Duration::from_secs(5);
    let before = f.backend.metadata_calls.load(Ordering::SeqCst);
    assert_eq!(
        f.vfs.list(&RemotePath::root()).unwrap()[0].name,
        "cached.txt"
    );
    assert_eq!(f.backend.metadata_calls.load(Ordering::SeqCst), before + 1);
    assert_eq!(f.vfs.stat(&p("/cached.txt")).unwrap().1.size, 3);
    assert_eq!(f.vfs.list(&RemotePath::root()).unwrap().len(), 1);
    assert!(matches!(
        f.vfs.list(&p("/uncached")),
        Err(Error::Network(_))
    ));
    assert_eq!(f.backend.metadata_calls.load(Ordering::SeqCst), before + 1);
    *f.backend.metadata_delay.lock() = Duration::ZERO;
    std::fs::write(f.root.path().join("cached.txt"), b"updated").unwrap();
    *f.vfs.s.offline_until.lock() = Some(Instant::now() - Duration::from_secs(1));
    assert_eq!(f.vfs.stat(&p("/cached.txt")).unwrap().1.size, 7);
    assert!(f.vfs.s.offline_until.lock().is_none());
}

#[test]
fn local_changes_survive_offline_cooldown_listing() {
    let f = fixture_with(|o| o.meta_db = Some(o.staging_dir.with_file_name("meta.db")));
    std::fs::create_dir(f.root.path().join("Krea2")).unwrap();
    f.vfs.list(&RemotePath::root()).unwrap();
    // 원격 확인 대기 중에도 쓰기는 원격으로 바로 갑니다. 목록만 저장본으로 폴백합니다.
    *f.vfs.s.offline_until.lock() = Some(Instant::now() + Duration::from_secs(60));
    let names = |f: &Fixture| -> Vec<String> {
        f.vfs.refresh();
        let mut v: Vec<String> = f
            .vfs
            .list(&RemotePath::root())
            .unwrap()
            .iter()
            .map(|e| e.name.clone())
            .collect();
        v.sort();
        v
    };

    let h = f.vfs.create(&p("/새 폴더"), EntryKind::Dir).unwrap();
    f.vfs.cleanup(&h, false).unwrap();
    f.vfs.close(&h);
    assert_eq!(names(&f), ["Krea2", "새 폴더"]);

    let h = f.vfs.open(&p("/새 폴더")).unwrap();
    f.vfs.rename(&h, &p("/Flux1.D"), false).unwrap();
    f.vfs.cleanup(&h, false).unwrap();
    f.vfs.close(&h);
    assert_eq!(names(&f), ["Flux1.D", "Krea2"]);

    let h = f.vfs.open(&p("/Flux1.D")).unwrap();
    f.vfs.cleanup(&h, true).unwrap();
    f.vfs.close(&h);
    assert_eq!(names(&f), ["Krea2"]);
}

#[test]
fn rename_target_parent_uses_real_case() {
    let f = fixture(false);
    std::fs::create_dir(f.root.path().join("loras")).unwrap();
    std::fs::create_dir(f.root.path().join("loras/새 폴더")).unwrap();
    f.vfs.list(&RemotePath::root()).unwrap();
    let h = f.vfs.open(&p("/loras/새 폴더")).unwrap();
    // WinFsp가 넘기는 형태: 상위 경로만 대문자
    f.vfs.rename(&h, &p("/LORAS/Flux1.D"), false).unwrap();
    assert_eq!(h.path().as_str(), "/loras/Flux1.D");
    f.vfs.cleanup(&h, false).unwrap();
    f.vfs.close(&h);
    assert!(f.root.path().join("loras/Flux1.D").is_dir());
}

#[test]
fn authentication_and_not_found_do_not_trigger_offline_cooldown() {
    let f = fixture(false);
    let result: Result<()> = f._rt.block_on(
        f.vfs
            .s
            .remote_read(async { Err(Error::Auth("test".into())) }, true),
    );
    assert!(matches!(result, Err(Error::Auth(_))));
    assert!(matches!(f.vfs.stat(&p("/missing")), Err(Error::NotFound)));
    assert!(f.vfs.s.offline_until.lock().is_none());
}

#[test]
fn shutdown_releases_backend_held_by_directory_watcher() {
    let f = fixture(false);
    f.vfs.list(&RemotePath::root()).unwrap();
    *f.backend.metadata_delay.lock() = Duration::from_secs(5);
    let before = f.backend.metadata_calls.load(Ordering::SeqCst);
    f.vfs.start_watch(Duration::from_millis(1));
    for _ in 0..100 {
        if f.backend.metadata_calls.load(Ordering::SeqCst) > before {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(f.backend.metadata_calls.load(Ordering::SeqCst) > before);
    let backend = f.backend.clone();
    f.vfs.shutdown();
    drop(f.vfs);
    assert_eq!(
        Arc::strong_count(&backend),
        2,
        "해제 후 백그라운드 작업이 백엔드를 보관합니다"
    );
}
