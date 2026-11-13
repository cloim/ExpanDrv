use super::*;
use expandrv_backends::local::LocalBackend;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::time::UNIX_EPOCH;

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
    /// 연 스트리밍 세션 수와 세션으로 보낸 쓰기 수
    sessions: AtomicUsize,
    session_writes: Arc<AtomicUsize>,
    /// 다음 세션 쓰기 몇 번을 일시 오류로 실패시킵니다.
    fail_session_writes: Arc<AtomicU32>,
    /// 임의 오프셋 세션 지원 여부(기본 true)
    random_write: AtomicBool,
    /// 쓰기 성능 비교용 망 모형: 왕복 지연과 업로드 대역폭(바이트/초, 0이면 무제한)
    net_rtt: Mutex<Duration>,
    net_bps: Arc<AtomicU64>,
}

/// 망 모형에서 `len`바이트를 보내는 시간
fn transfer_time(bps: &AtomicU64, len: u64) -> Duration {
    match bps.load(Ordering::SeqCst) {
        0 => Duration::ZERO,
        n => Duration::from_secs_f64(len as f64 / n as f64),
    }
}

/// 실패 주입과 계수를 더한 시험용 세션
struct TestUpload {
    inner: Box<dyn UploadSession>,
    writes: Arc<AtomicUsize>,
    fail: Arc<AtomicU32>,
    rtt: Duration,
    bps: Arc<AtomicU64>,
    /// 아직 기다리지 않은 전송 시간. 짧은 대기를 모아 타이머 해상도 왜곡을 줄입니다.
    debt: Duration,
}

#[async_trait::async_trait]
impl UploadSession for TestUpload {
    async fn write(&mut self, offset: u64, data: Bytes) -> Result<()> {
        if self
            .fail
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(Error::Network("injected session".into()));
        }
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.debt += transfer_time(&self.bps, data.len() as u64);
        if self.debt >= Duration::from_millis(100) {
            tokio::time::sleep(std::mem::take(&mut self.debt)).await;
        }
        self.inner.write(offset, data).await
    }
    async fn flush(&mut self) -> Result<()> {
        self.inner.flush().await
    }
    async fn finish(self: Box<Self>) -> Result<Entry> {
        // SFTP 기준 닫기, 교체, 조회 왕복 3회와 남은 전송 시간
        tokio::time::sleep(self.rtt * 3 + self.debt).await;
        self.inner.finish().await
    }
    async fn abort(self: Box<Self>) {
        self.inner.abort().await;
    }
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
            random_write: self.random_write.load(Ordering::SeqCst),
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
        // SFTP 기준 대상 확인, 열기, 닫기, 교체, 조회 왕복 5회에 본문 전송 시간을 더합니다.
        let rtt = *self.net_rtt.lock();
        tokio::time::sleep(rtt * 5 + transfer_time(&self.net_bps, source.len())).await;
        self.inner.write(path, source).await
    }
    async fn begin_upload(
        &self,
        path: &RemotePath,
        hint: UploadHint,
    ) -> Result<Box<dyn UploadSession>> {
        self.check()?;
        self.sessions.fetch_add(1, Ordering::SeqCst);
        let rtt = *self.net_rtt.lock();
        // SFTP 기준 대상 확인과 임시 파일 열기. 새 파일이면 확인을 생략합니다.
        tokio::time::sleep(if hint.new_file { rtt } else { rtt * 2 }).await;
        let inner = self.inner.begin_upload(path, hint).await?;
        Ok(Box::new(TestUpload {
            inner,
            writes: self.session_writes.clone(),
            fail: self.fail_session_writes.clone(),
            rtt,
            bps: self.net_bps.clone(),
            debt: Duration::ZERO,
        }))
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
    async fn set_mtime(&self, path: &RemotePath, mtime: SystemTime) -> Result<()> {
        self.inner.set_mtime(path, mtime).await
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
        sessions: AtomicUsize::new(0),
        session_writes: Arc::new(AtomicUsize::new(0)),
        fail_session_writes: Arc::new(AtomicU32::new(0)),
        random_write: AtomicBool::new(true),
        net_rtt: Mutex::new(Duration::ZERO),
        net_bps: Arc::new(AtomicU64::new(0)),
    });
    let mut opts = VfsOptions::new(staging.path().join("stage"));
    opts.metadata_ttl = Duration::from_secs(60);
    opts.op_timeout = Duration::from_secs(10);
    // 기존 시험은 백그라운드 업로드 동작을 검증합니다. 실시간 모드는 `through()`를 씁니다.
    opts.write_mode = WriteMode::Back;
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

/// 실시간 업로드(write-through) 픽스처
fn through() -> Fixture {
    fixture_with(|o| o.write_mode = WriteMode::Through)
}

fn remote(f: &Fixture, name: &str) -> Vec<u8> {
    std::fs::read(f.root.path().join(name)).unwrap()
}

/// 조건이 참이 될 때까지 최대 3초 기다립니다. 세션 중단 정리는 비동기로 끝납니다.
fn wait_until(mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !cond() {
        assert!(Instant::now() < deadline, "조건이 제때 충족되지 않았습니다");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 원격 루트의 이름 목록. 세션 임시 파일이 남지 않았는지 확인할 때 씁니다.
fn remote_names(f: &Fixture) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(f.root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    v.sort();
    v
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
fn through_write_streams_and_cleanup_waits_for_remote() {
    let f = through();
    let h = f.vfs.create(&p("/a.txt"), EntryKind::File).unwrap();
    let full_uploads = f.backend.writes.load(Ordering::SeqCst);
    f.vfs.write(&h, 0, b"hello", false, false).unwrap();
    f.vfs.write(&h, 0, b" world", true, false).unwrap();
    assert_eq!(f.backend.sessions.load(Ordering::SeqCst), 1);
    assert_eq!(f.vfs.stat(&p("/a.txt")).unwrap().1.size, 11);

    // 동기화 호출 없이 정리가 돌아오면 원격에 반영되어 있습니다.
    f.vfs.cleanup(&h, false).unwrap();
    assert_eq!(remote(&f, "a.txt"), b"hello world");
    assert_eq!(f.vfs.pending_uploads(), 0);
    assert_eq!(staging_files(&f), 0);
    assert_eq!(f.backend.writes.load(Ordering::SeqCst), full_uploads);
    assert_eq!(f.backend.session_writes.load(Ordering::SeqCst), 2);
    assert_eq!(
        f.vfs.take_transfer_events(),
        vec![TransferEvent::Uploaded(p("/a.txt"))]
    );
    f.vfs.close(&h);
    assert_eq!(remote_names(&f), ["a.txt"]);
}

#[test]
fn through_create_defers_remote_file_until_commit() {
    let f = through();
    f.vfs.list(&RemotePath::root()).unwrap();
    let h = f.vfs.create(&p("/new.txt"), EntryKind::File).unwrap();
    // 원격에는 아직 없지만 목록과 속성에는 보입니다.
    assert_eq!(f.backend.started_writes.load(Ordering::SeqCst), 0);
    assert!(remote_names(&f).is_empty());
    assert_eq!(f.vfs.stat(&p("/new.txt")).unwrap().1.size, 0);
    let names: Vec<String> = f
        .vfs
        .list(&RemotePath::root())
        .unwrap()
        .iter()
        .map(|e| e.name.clone())
        .collect();
    assert_eq!(names, ["new.txt"]);
    assert!(matches!(
        f.vfs.create(&p("/new.txt"), EntryKind::File),
        Err(Error::AlreadyExists)
    ));
    // 원격 변경 확인이 쓰는 중인 새 파일을 사라진 것으로 알리지 않습니다.
    assert!(f.vfs.poll_changes().is_empty());
    assert_eq!(
        f.vfs.upload_counts(),
        UploadCounts {
            uploading: 1,
            ..Default::default()
        }
    );

    // 데이터 없이 닫아도 빈 파일을 한 번에 만듭니다.
    f.vfs.cleanup(&h, false).unwrap();
    f.vfs.close(&h);
    assert_eq!(remote(&f, "new.txt"), b"");
    assert_eq!(f.vfs.upload_counts(), UploadCounts::default());
    assert_eq!(f.backend.started_writes.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.vfs.take_transfer_events(),
        vec![TransferEvent::Uploaded(p("/new.txt"))]
    );
    assert!(f.vfs.poll_changes().is_empty());
}

#[test]
fn through_local_only_rename_and_replace_happen_at_commit() {
    let f = through();
    std::fs::write(f.root.path().join("doc.txt"), b"old").unwrap();
    // 저장 프로그램처럼 임시 이름으로 쓰고 원래 이름으로 바꿉니다.
    let h = f.vfs.create(&p("/doc.tmp"), EntryKind::File).unwrap();
    f.vfs.write(&h, 0, b"new", false, false).unwrap();
    assert!(matches!(
        f.vfs.rename(&h, &p("/doc.txt"), false),
        Err(Error::AlreadyExists)
    ));
    f.vfs.rename(&h, &p("/doc.txt"), true).unwrap();
    assert_eq!(h.path().as_str(), "/doc.txt");
    // 확정 전에는 원격 원본이 그대로입니다.
    assert_eq!(remote(&f, "doc.txt"), b"old");
    f.vfs.cleanup(&h, false).unwrap();
    f.vfs.close(&h);
    assert_eq!(remote(&f, "doc.txt"), b"new");
    wait_until(|| remote_names(&f) == ["doc.txt"]);
    assert!(matches!(f.vfs.stat(&p("/doc.tmp")), Err(Error::NotFound)));
}

/// 파일 시스템 요청이 상태 잠금이나 노드 표 잠금을 쥔 채 원격 응답을 기다리는 동안에도
/// 에이전트의 상태 집계는 막히지 않아야 합니다(막히면 런타임 입출력이 멈춰 교착에 빠집니다).
#[test]
fn upload_counts_never_blocks_on_busy_locks() {
    let f = through();
    let h = f.vfs.create(&p("/busy.bin"), EntryKind::File).unwrap();
    f.vfs.write(&h, 0, b"x", false, false).unwrap();
    let expected = UploadCounts {
        uploading: 1,
        ..Default::default()
    };
    assert_eq!(f.vfs.upload_counts(), expected);

    let counts_from_other_thread = |f: &Fixture| {
        let (tx, rx) = std::sync::mpsc::channel();
        let vfs = f.vfs.clone();
        std::thread::spawn(move || {
            let _ = tx.send(vfs.upload_counts());
        });
        rx.recv_timeout(Duration::from_secs(2))
            .expect("상태 집계가 잠금에서 기다렸습니다")
    };
    {
        let _busy_state = h.node.state.lock();
        assert_eq!(counts_from_other_thread(&f), expected);
    }
    {
        let _busy_nodes = f.vfs.s.nodes.lock();
        assert_eq!(counts_from_other_thread(&f), expected);
    }
    f.vfs.cleanup(&h, false).unwrap();
    f.vfs.close(&h);
    assert_eq!(f.vfs.upload_counts(), UploadCounts::default());
}

#[test]
fn upload_counts_separate_active_waiting_and_failed() {
    // 백그라운드 모드: 닫기 전 변경은 대기, 실패한 업로드는 실패로 셉니다.
    let f = fixture(false);
    *f.backend.write_delay.lock() = Duration::from_millis(300);
    f.backend.fail_writes.store(1, Ordering::SeqCst);
    std::fs::write(f.root.path().join("w.txt"), b"x").unwrap();
    let h = f.vfs.open(&p("/w.txt")).unwrap();
    f.vfs.write(&h, 0, b"y", false, false).unwrap();
    assert_eq!(
        f.vfs.upload_counts(),
        UploadCounts {
            waiting: 1,
            ..Default::default()
        }
    );
    f.vfs.cleanup(&h, false).unwrap();
    // 첫 시도가 전송 중이면 업로드 중입니다.
    wait_until(|| f.vfs.upload_counts().uploading == 1);
    // 첫 시도가 실패하면 다시 시도할 때까지 실패로 보입니다.
    wait_until(|| f.vfs.upload_counts().failed == 1);
    sync(&f);
    assert_eq!(f.vfs.upload_counts(), UploadCounts::default());
}

/// 캐시 관리자가 같은 파일의 쓰기를 여러 스레드로 동시에 내보내도, 조각이 확정보다 늦게 도착해
/// 취소 오류가 나거나 조각이 빠진 채 확정되지 않아야 합니다.
#[test]
fn through_concurrent_writes_keep_chunk_and_commit_order() {
    for random in [true, false] {
        for round in 0..20 {
            let f = through();
            f.backend.random_write.store(random, Ordering::SeqCst);
            let name = format!("c-{round}.bin");
            let h = f
                .vfs
                .create(&p(&format!("/{name}")), EntryKind::File)
                .unwrap();
            let chunk = 8 * 1024usize;
            let data: Vec<u8> = (0..chunk * 8).map(|i| (i % 251) as u8).collect();
            f.vfs.set_size(&h, data.len() as u64, false).unwrap();
            std::thread::scope(|scope| {
                for part in 0..8 {
                    let (vfs, h, data) = (&f.vfs, &h, &data);
                    scope.spawn(move || {
                        let off = part * chunk;
                        vfs.write(h, off as u64, &data[off..off + chunk], false, true)
                            .unwrap();
                    });
                }
            });
            f.vfs.cleanup(&h, false).unwrap();
            f.vfs.close(&h);
            assert_eq!(remote(&f, &name), data, "random={random} round={round}");
            assert_eq!(f.vfs.pending_uploads(), 0);
        }
    }
}

#[test]
fn through_early_commit_on_size_hint() {
    let f = through();
    let h = f.vfs.create(&p("/b.bin"), EntryKind::File).unwrap();
    // 탐색기는 데이터를 쓰기 전에 최종 크기를 설정합니다.
    assert_eq!(f.vfs.set_size(&h, 11, false).unwrap().size, 11);
    f.vfs.write(&h, 0, b"hello ", false, false).unwrap();
    assert_eq!(f.vfs.pending_uploads(), 1);
    f.vfs.write(&h, 6, b"world", false, false).unwrap();
    // 마지막 바이트를 쓴 호출 안에서 확정됩니다.
    assert_eq!(remote(&f, "b.bin"), b"hello world");
    assert_eq!(f.vfs.pending_uploads(), 0);

    let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    f.vfs.set_mtime(&h, t).unwrap();
    assert_eq!(
        std::fs::metadata(f.root.path().join("b.bin"))
            .unwrap()
            .modified()
            .unwrap(),
        t
    );
    f.vfs.cleanup(&h, false).unwrap();
    f.vfs.close(&h);
    assert_eq!(
        f.vfs.take_transfer_events(),
        vec![TransferEvent::Uploaded(p("/b.bin"))]
    );
    assert_eq!(remote_names(&f), ["b.bin"]);
}

#[test]
fn through_flush_waits_for_sent_data() {
    let f = through();
    *f.backend.write_delay.lock() = Duration::ZERO;
    let h = f.vfs.create(&p("/fl"), EntryKind::File).unwrap();
    f.vfs
        .write(&h, 0, &vec![1u8; 300_000], false, false)
        .unwrap();
    f.vfs.flush(&h).unwrap();
    assert_eq!(f.backend.session_writes.load(Ordering::SeqCst), 1);
    f.vfs.cleanup(&h, false).unwrap();
    assert_eq!(remote(&f, "fl").len(), 300_000);
}

#[test]
fn through_stream_error_is_returned_to_caller_then_cleanup_uploads_fully() {
    let f = through();
    f.backend.fail_session_writes.store(1, Ordering::SeqCst);
    let h = f.vfs.create(&p("/c"), EntryKind::File).unwrap();
    let full_uploads = f.backend.writes.load(Ordering::SeqCst);
    f.vfs.write(&h, 0, b"abc", false, false).unwrap();
    // 실패는 다음 동기 지점에서 호출 프로그램에 드러납니다.
    assert!(matches!(f.vfs.flush(&h), Err(Error::Network(_))));
    // 이후 쓰기는 스테이징에만 남고, 정리 때 전체 업로드로 반영됩니다.
    f.vfs.write(&h, 3, b"def", false, false).unwrap();
    assert_eq!(f.backend.sessions.load(Ordering::SeqCst), 1);
    f.vfs.cleanup(&h, false).unwrap();
    assert_eq!(remote(&f, "c"), b"abcdef");
    assert_eq!(f.backend.writes.load(Ordering::SeqCst), full_uploads + 1);
    assert_eq!(f.vfs.pending_uploads(), 0);
    assert_eq!(
        f.vfs.take_transfer_events(),
        vec![TransferEvent::Uploaded(p("/c"))]
    );
    wait_until(|| remote_names(&f) == ["c"]);
}

#[test]
fn through_out_of_order_write_falls_back_to_full_upload() {
    let f = through();
    f.backend.random_write.store(false, Ordering::SeqCst);
    let h = f.vfs.create(&p("/d"), EntryKind::File).unwrap();
    f.vfs.write(&h, 0, b"aaaa", false, false).unwrap();
    f.vfs.write(&h, 8, b"bb", false, false).unwrap();
    f.vfs.cleanup(&h, false).unwrap();
    assert_eq!(remote(&f, "d"), b"aaaa\0\0\0\0bb");
    assert_eq!(f.backend.sessions.load(Ordering::SeqCst), 1);
    wait_until(|| remote_names(&f) == ["d"]);
}

#[test]
fn through_size_change_after_sending_falls_back() {
    let f = through();
    let h = f.vfs.create(&p("/t"), EntryKind::File).unwrap();
    f.vfs.write(&h, 0, b"0123456789", false, false).unwrap();
    assert_eq!(f.vfs.set_size(&h, 4, false).unwrap().size, 4);
    f.vfs.cleanup(&h, false).unwrap();
    assert_eq!(remote(&f, "t"), b"0123");
    wait_until(|| remote_names(&f) == ["t"]);
}

#[test]
fn through_partial_edit_uploads_synchronously_on_cleanup() {
    let f = through();
    std::fs::write(f.root.path().join("e.bin"), b"0123456789").unwrap();
    let h = f.vfs.open(&p("/e.bin")).unwrap();
    f.vfs.write(&h, 2, b"AB", false, false).unwrap();
    f.vfs.cleanup(&h, false).unwrap();
    assert_eq!(remote(&f, "e.bin"), b"01AB456789");
    assert_eq!(f.backend.sessions.load(Ordering::SeqCst), 0);
    assert_eq!(f.vfs.pending_uploads(), 0);
}

#[test]
fn through_cleanup_commit_failure_notifies_and_retries_in_background() {
    let f = through();
    let h = f.vfs.create(&p("/x"), EntryKind::File).unwrap();
    f.backend.fail_session_writes.store(1, Ordering::SeqCst);
    f.backend.fail_writes.store(1, Ordering::SeqCst);
    f.vfs.write(&h, 0, b"x", false, false).unwrap();
    // 세션과 전체 업로드가 모두 실패해도 정리는 돌아오고, 실패를 알린 뒤 다시 시도합니다.
    f.vfs.cleanup(&h, false).unwrap();
    assert_eq!(f.vfs.pending_uploads(), 1);
    assert!(matches!(
        f.vfs.take_transfer_events().as_slice(),
        [TransferEvent::Failed(path, _)] if *path == p("/x")
    ));
    f.vfs.close(&h);
    sync(&f);
    assert_eq!(remote(&f, "x"), b"x");
    assert_eq!(f.vfs.pending_uploads(), 0);
    assert_eq!(
        f.vfs.take_transfer_events(),
        vec![TransferEvent::Uploaded(p("/x"))]
    );
}

#[test]
fn through_delete_while_streaming_discards_session() {
    let f = through();
    let h = f.vfs.create(&p("/del"), EntryKind::File).unwrap();
    f.vfs.write(&h, 0, b"gone", false, false).unwrap();
    f.vfs.check_delete(&h).unwrap();
    f.vfs.cleanup(&h, true).unwrap();
    f.vfs.close(&h);
    assert!(matches!(f.vfs.stat(&p("/del")), Err(Error::NotFound)));
    assert_eq!(f.vfs.pending_uploads(), 0);
    wait_until(|| remote_names(&f).is_empty());
}

#[test]
fn through_rename_while_streaming_commits_to_new_path() {
    let f = through();
    let h = f.vfs.create(&p("/old"), EntryKind::File).unwrap();
    f.vfs.write(&h, 0, b"moved", false, false).unwrap();
    f.vfs.rename(&h, &p("/new"), false).unwrap();
    f.vfs.cleanup(&h, false).unwrap();
    assert_eq!(remote(&f, "new"), b"moved");
    wait_until(|| remote_names(&f) == ["new"]);
}

#[test]
fn through_concurrent_files_stream_within_slots_and_rest_fall_back() {
    let f = through();
    let handles: Vec<_> = (0..3)
        .map(|i| {
            f.vfs
                .create(&p(&format!("/p{i}")), EntryKind::File)
                .unwrap()
        })
        .collect();
    for (i, h) in handles.iter().enumerate() {
        f.vfs
            .write(h, 0, format!("file-{i}").as_bytes(), false, false)
            .unwrap();
    }
    // 세션 슬롯(2개)을 넘는 파일은 정리 때 전체 업로드로 반영됩니다.
    assert_eq!(f.backend.sessions.load(Ordering::SeqCst), 2);
    for h in &handles {
        f.vfs.cleanup(h, false).unwrap();
    }
    for i in 0..3 {
        assert_eq!(remote(&f, &format!("p{i}")), format!("file-{i}").as_bytes());
    }
    assert_eq!(f.vfs.pending_uploads(), 0);
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
fn slow_probe_during_active_transfer_does_not_start_offline_cooldown() {
    let f = fixture_with(|o| o.meta_db = Some(o.staging_dir.with_file_name("meta.db")));
    std::fs::write(f.root.path().join("a.txt"), b"a").unwrap();
    f.vfs.list(&RemotePath::root()).unwrap();
    // 큰 업로드 중 서버가 목록 조회에 늦게 답하는 상황: 저장본으로 답하고 오프라인 대기는 시작하지 않습니다.
    f.vfs.s.mark_alive();
    *f.backend.metadata_delay.lock() = Duration::from_millis(2500);
    f.vfs.refresh();
    let names: Vec<String> = f
        .vfs
        .list(&RemotePath::root())
        .unwrap()
        .iter()
        .map(|e| e.name.clone())
        .collect();
    assert_eq!(names, ["a.txt"]);
    assert!(f.vfs.s.offline_until.lock().is_none());

    // 최근 성공 기록이 오래되었으면 이전처럼 오프라인 대기로 들어갑니다.
    *f.vfs.s.alive_at.lock() = Instant::now().checked_sub(Duration::from_secs(60));
    f.vfs.refresh();
    f.vfs.list(&RemotePath::root()).unwrap();
    assert!(f.vfs.s.offline_until.lock().is_some());
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

// ---------- 실제 백엔드 대상 실시간 업로드 종단 시험(환경 변수가 없으면 건너뜁니다) ----------

mod live {
    use super::*;
    use expandrv_core::config::{
        BackendConfig, EnvSecrets, FtpConfig, FtpTls, SecretSource, SftpConfig, WebDavConfig,
    };

    fn env(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }

    fn sftp() -> Option<BackendConfig> {
        Some(BackendConfig::Sftp(SftpConfig {
            ssh_shell: false,
            use_ssh_config: false,
            host: env("EXPANDRV_TEST_SFTP_HOST")?,
            port: env("EXPANDRV_TEST_SFTP_PORT")?.parse().ok()?,
            username: env("EXPANDRV_TEST_SFTP_USER")?,
            password: Some(SecretSource::Env {
                var: "EXPANDRV_TEST_SFTP_PASSWORD".into(),
            }),
            private_key: None,
            key_passphrase: None,
            root: env("EXPANDRV_TEST_SFTP_ROOT").unwrap_or_else(|| "/data".into()),
            known_hosts: Some(env("EXPANDRV_TEST_SFTP_KNOWN_HOSTS")?.into()),
        }))
    }

    fn ftp() -> Option<BackendConfig> {
        Some(BackendConfig::Ftp(FtpConfig {
            pinned_cert_sha256: None,
            host: env("EXPANDRV_TEST_FTP_HOST")?,
            port: env("EXPANDRV_TEST_FTP_PORT")?.parse().ok()?,
            username: env("EXPANDRV_TEST_FTP_USER").unwrap_or_else(|| "u".into()),
            password: Some(SecretSource::Env {
                var: "EXPANDRV_TEST_FTP_PASS".into(),
            }),
            tls: FtpTls::None,
            encoding: "utf-8".into(),
            root: env("EXPANDRV_TEST_FTP_ROOT").unwrap_or_else(|| ".".into()),
        }))
    }

    fn webdav() -> Option<BackendConfig> {
        Some(BackendConfig::Webdav(WebDavConfig {
            url: env("EXPANDRV_TEST_WEBDAV_URL")?,
            username: Some(env("EXPANDRV_TEST_WEBDAV_USER").unwrap_or_else(|| "u".into())),
            password: Some(SecretSource::Env {
                var: "EXPANDRV_TEST_WEBDAV_PASS".into(),
            }),
            pinned_cert_sha256: None,
        }))
    }

    /// 탐색기 복사 순서(생성, 크기 설정, 순차 쓰기, 정리)를 VFS 실시간 모드로 수행하고 원격 내용을 대조합니다.
    fn exercise(tag: &str, cfg: BackendConfig) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let backend = rt
            .block_on(expandrv_backends::connect(&cfg, None, &EnvSecrets))
            .unwrap();
        assert!(backend.capabilities().streaming_upload, "{tag}");
        let staging = tempfile::tempdir().unwrap();
        let mut opts = VfsOptions::new(staging.path().join("stage"));
        opts.op_timeout = Duration::from_secs(30);
        let vfs = Vfs::new(backend.clone(), rt.handle().clone(), opts).unwrap();

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = p(&format!("/vfs-{tag}-{nanos:x}"));
        let dh = vfs.create(&dir, EntryKind::Dir).unwrap();
        vfs.cleanup(&dh, false).unwrap();
        let file = dir.join("big.bin").unwrap();
        let data: Vec<u8> = (0..(3 << 20) + 123).map(|i| (i * 31 % 251) as u8).collect();

        let h = vfs.create(&file, EntryKind::File).unwrap();
        vfs.set_size(&h, data.len() as u64, false).unwrap();
        let mut off = 0usize;
        for chunk in data.chunks(1 << 20) {
            vfs.write(&h, off as u64, chunk, false, false).unwrap();
            off += chunk.len();
        }
        // 크기 힌트와 같아진 마지막 쓰기 안에서 확정됩니다.
        assert_eq!(vfs.pending_uploads(), 0, "{tag}: 조기 커밋");
        vfs.cleanup(&h, false).unwrap();
        vfs.close(&h);
        let got = rt
            .block_on(backend.read_range(&file, 0, data.len() as u64))
            .unwrap();
        assert_eq!(&got[..], &data[..], "{tag}: 원격 내용");
        let names: Vec<String> = rt
            .block_on(backend.list(&dir))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["big.bin"], "{tag}: 임시 파일 흔적");

        // 기존 파일 일부 수정은 정리 때 동기 전체 업로드로 반영됩니다.
        let h = vfs.open(&file).unwrap();
        vfs.write(&h, 100, b"ZZZ", false, false).unwrap();
        vfs.cleanup(&h, false).unwrap();
        vfs.close(&h);
        assert_eq!(vfs.pending_uploads(), 0, "{tag}: 부분 수정 동기 반영");
        let got = rt.block_on(backend.read_range(&file, 100, 3)).unwrap();
        assert_eq!(&got[..], b"ZZZ", "{tag}");
        assert_eq!(
            vfs.take_transfer_events(),
            vec![
                TransferEvent::Uploaded(file.clone()),
                TransferEvent::Uploaded(file.clone())
            ]
        );

        let h = vfs.open(&file).unwrap();
        vfs.check_delete(&h).unwrap();
        vfs.cleanup(&h, true).unwrap();
        vfs.close(&h);
        let dh = vfs.open(&dir).unwrap();
        vfs.check_delete(&dh).unwrap();
        vfs.cleanup(&dh, true).unwrap();
        vfs.close(&dh);
        vfs.shutdown();
    }

    #[test]
    fn through_mode_against_sftp() {
        match sftp() {
            Some(cfg) => exercise("sftp", cfg),
            None => eprintln!("EXPANDRV_TEST_SFTP_*가 없어 건너뜁니다"),
        }
    }

    #[test]
    fn through_mode_against_ftp() {
        match ftp() {
            Some(cfg) => exercise("ftp", cfg),
            None => eprintln!("EXPANDRV_TEST_FTP_*가 없어 건너뜁니다"),
        }
    }

    #[test]
    fn through_mode_against_webdav() {
        match webdav() {
            Some(cfg) => exercise("webdav", cfg),
            None => eprintln!("EXPANDRV_TEST_WEBDAV_URL이 없어 건너뜁니다"),
        }
    }
}
