use super::*;
use expandrv_backends::local::LocalBackend;
use std::sync::atomic::{AtomicBool, AtomicU32};

/// 지연, 오프라인, 실패와 호출 횟수를 흉내 내는 시험용 백엔드
struct TestBackend {
    inner: LocalBackend,
    write_delay: Mutex<Duration>,
    offline: AtomicBool,
    fail_writes: AtomicU32,
    reads: AtomicUsize,
    writes: AtomicUsize,
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
        self.inner.capabilities()
    }
    async fn stat(&self, path: &RemotePath) -> Result<Entry> {
        self.check()?;
        self.inner.stat(path).await
    }
    async fn list(&self, path: &RemotePath) -> Result<Vec<Entry>> {
        self.check()?;
        self.inner.list(path).await
    }
    async fn read_range(&self, path: &RemotePath, offset: u64, len: u64) -> Result<Bytes> {
        self.check()?;
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.read_range(path, offset, len).await
    }
    async fn write(&self, path: &RemotePath, source: WriteSource) -> Result<Entry> {
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
        offline: AtomicBool::new(false),
        fail_writes: AtomicU32::new(0),
        reads: AtomicUsize::new(0),
        writes: AtomicUsize::new(0),
    });
    let mut opts = VfsOptions::new(staging.path().join("stage"));
    opts.metadata_ttl = Duration::from_secs(60);
    opts.op_timeout = Duration::from_secs(10);
    tune(&mut opts);
    let vfs = Vfs::new(backend.clone(), rt.handle().clone(), opts).unwrap();
    Fixture {
        _rt: rt,
        root,
        staging,
        backend,
        vfs,
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
