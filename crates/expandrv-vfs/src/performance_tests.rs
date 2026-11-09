use super::*;

fn finish_prefetch(f: &Fixture) {
    let until = Instant::now() + Duration::from_secs(5);
    while f
        .vfs
        .s
        .background
        .lock()
        .iter()
        .filter(|t| !t.is_finished())
        .count()
        > 1
    {
        assert!(Instant::now() < until, "선행 읽기 종료 대기");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn header_reads_do_not_download_neighboring_blocks() {
    let f = fixture_with(|o| o.block_size = 64 * 1024);
    std::fs::write(f.root.path().join("header"), vec![9; 512 * 1024]).unwrap();
    let h = f.vfs.open(&p("/header")).unwrap();
    let mut buf = [0; 4096];
    assert_eq!(f.vfs.read(&h, 0, &mut buf).unwrap(), buf.len());
    finish_prefetch(&f);
    assert_eq!(f.backend.reads.load(Ordering::SeqCst), 1);
    assert!(buf.iter().all(|b| *b == 9));
    f.vfs.cleanup(&h, false).unwrap();
    f.vfs.close(&h);
}

#[test]
fn sequential_read_prefetches_and_seeks_reset_the_window() {
    let f = fixture_with(|o| o.block_size = 64 * 1024);
    std::fs::write(f.root.path().join("seq"), vec![7; 2 * 1024 * 1024]).unwrap();
    let h = f.vfs.open(&p("/seq")).unwrap();
    let mut buf = vec![0; 64 * 1024];
    f.vfs.read(&h, 0, &mut buf).unwrap();
    finish_prefetch(&f);
    assert!(
        f.vfs
            .s
            .blocks
            .get(&p("/seq"), &version(&h.node.state.lock().entry), 1)
            .is_some()
    );
    let before = f.backend.reads.load(Ordering::SeqCst);
    f.vfs.read(&h, 1024 * 1024, &mut [0; 1024]).unwrap();
    finish_prefetch(&f);
    assert_eq!(f.backend.reads.load(Ordering::SeqCst), before + 1);
    assert_eq!(f.vfs.active_downloads(), 0);
    f.vfs.cleanup(&h, false).unwrap();
    f.vfs.close(&h);
}

#[test]
fn shutdown_releases_active_prefetch_counts() {
    let f = fixture_with(|o| o.block_size = 64 * 1024);
    *f.backend.read_delay.lock() = Duration::from_millis(50);
    std::fs::write(f.root.path().join("stop"), vec![1; 512 * 1024]).unwrap();
    let h = f.vfs.open(&p("/stop")).unwrap();
    f.vfs.read(&h, 0, &mut vec![0; 64 * 1024]).unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while f.vfs.active_downloads() == 0 {
        assert!(Instant::now() < until);
        std::thread::yield_now();
    }
    f.vfs.shutdown();
    while f.vfs.active_downloads() != 0 {
        assert!(Instant::now() < until, "취소된 다운로드 계수 정리");
        std::thread::yield_now();
    }
}

async fn mixed_latency(f: &Fixture, scheduled: bool) -> Duration {
    let (foreground, background) = if scheduled {
        let [fg, bg, _] = scheduler::schedule(f.backend.clone());
        (fg, bg)
    } else {
        (
            f.backend.clone() as Arc<dyn Backend>,
            f.backend.clone() as Arc<dyn Backend>,
        )
    };
    let mut tasks = Vec::new();
    for i in 0..4 {
        let bg = background.clone();
        tasks.push(tokio::spawn(async move {
            bg.write(
                &p(&format!("/upload-{i}")),
                WriteSource::Bytes(Bytes::from_static(b"upload")),
            )
            .await
            .unwrap();
        }));
    }
    let started = if scheduled { 2 } else { 4 };
    tokio::time::timeout(Duration::from_secs(2), async {
        while f.backend.started_writes.load(Ordering::SeqCst) < started {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let at = Instant::now();
    tokio::time::timeout(Duration::from_secs(2), foreground.list(&RemotePath::root()))
        .await
        .unwrap()
        .unwrap();
    let elapsed = at.elapsed();
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(
        f.backend.writes.load(Ordering::SeqCst),
        4,
        "백그라운드 업로드도 모두 완료"
    );
    elapsed
}

#[test]
fn foreground_queries_have_capacity_during_uploads_and_uploads_finish() {
    let f = fixture(false);
    *f.backend.write_delay.lock() = Duration::from_millis(150);
    let elapsed = f._rt.block_on(mixed_latency(&f, true));
    assert!(elapsed < Duration::from_millis(100), "{elapsed:?}");
}

/// 실제 서버 대신 4연결, 읽기 5ms, 업로드 150ms를 재현하는 통제 비교입니다.
#[test]
#[ignore = "통제 성능 비교: cargo test -p expandrv-vfs performance_comparison --release -- --ignored --nocapture"]
fn performance_comparison() {
    let mut counts = Vec::new();
    for legacy in [true, false] {
        let f = fixture_with(|o| {
            o.block_size = 64 * 1024;
            if legacy {
                o.readahead_blocks = 0;
            }
        });
        *f.backend.read_delay.lock() = Duration::from_millis(5);
        for i in 0..30 {
            let path = p(&format!("/file-{i}"));
            std::fs::write(
                f.root.path().join(format!("file-{i}")),
                vec![1; 1024 * 1024],
            )
            .unwrap();
            let h = f.vfs.open(&path).unwrap();
            f.vfs.read(&h, 0, &mut [0; 4096]).unwrap();
            if legacy {
                let ver = version(&h.node.state.lock().entry);
                f.vfs.s.prefetch(&path, &ver, 1, 1024 * 1024, 2);
            }
            finish_prefetch(&f);
            f.vfs.cleanup(&h, false).unwrap();
            f.vfs.close(&h);
        }
        counts.push(f.backend.reads.load(Ordering::SeqCst));
    }
    let mut latency = Vec::new();
    for scheduled in [false, true] {
        let mut times = Vec::new();
        for _ in 0..10 {
            let f = fixture(false);
            *f.backend.write_delay.lock() = Duration::from_millis(150);
            times.push(f._rt.block_on(mixed_latency(&f, scheduled)).as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        latency.push(times[9]);
    }
    println!(
        "PERFORMANCE {{\"header_requests_before\":{},\"header_requests_after\":{},\"header_bytes_before\":{},\"header_bytes_after\":{},\"mixed_query_p95_before_ms\":{:.3},\"mixed_query_p95_after_ms\":{:.3}}}",
        counts[0],
        counts[1],
        counts[0] * 65536,
        counts[1] * 65536,
        latency[0],
        latency[1]
    );
    assert_eq!(counts, [90, 30]);
    assert!(latency[1] < latency[0] / 2.0);
}
