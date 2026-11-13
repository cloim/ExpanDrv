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

#[test]
fn explorer_thumbnail_and_preview_patterns_avoid_redundant_fetches() {
    let f = fixture_with(|o| o.block_size = 64 * 1024);

    // 1. 대표 파일 형식 준비 (텍스트, 이미지, 동영상, 문서)
    let txt_path = f.root.path().join("text.txt");
    let jpg_path = f.root.path().join("photo.jpg");
    let mp4_path = f.root.path().join("movie.mp4");
    let doc_path = f.root.path().join("document.docx");

    std::fs::write(&txt_path, vec![1; 8 * 1024]).unwrap(); // 8 KiB
    std::fs::write(&jpg_path, vec![2; 2 * 1024 * 1024]).unwrap(); // 2 MiB
    std::fs::write(&mp4_path, vec![3; 50 * 1024 * 1024]).unwrap(); // 50 MiB
    std::fs::write(&doc_path, vec![4; 10 * 1024 * 1024]).unwrap(); // 10 MiB

    // (1) 텍스트 파일 미리보기 (앞부분 4KiB 읽기)
    let h_txt = f.vfs.open(&p("/text.txt")).unwrap();
    let mut buf = [0u8; 4096];
    assert_eq!(f.vfs.read(&h_txt, 0, &mut buf).unwrap(), 4096);
    finish_prefetch(&f);
    let reads_after_txt = f.backend.reads.load(Ordering::SeqCst);
    assert_eq!(reads_after_txt, 1, "텍스트 4KiB 읽기는 1개 블록만 요청");
    f.vfs.cleanup(&h_txt, false).unwrap();
    f.vfs.close(&h_txt);

    // (2) 이미지 파일 썸네일 (EXIF 헤더 64KiB 읽기 후 바로 닫기)
    let h_jpg = f.vfs.open(&p("/photo.jpg")).unwrap();
    let mut buf_64k = [0u8; 64 * 1024];
    assert_eq!(f.vfs.read(&h_jpg, 0, &mut buf_64k).unwrap(), 64 * 1024);
    // 핸들을 바로 닫아 썸네일러 동작 모사
    f.vfs.cleanup(&h_jpg, false).unwrap();
    f.vfs.close(&h_jpg);
    finish_prefetch(&f);
    let reads_after_jpg = f.backend.reads.load(Ordering::SeqCst);
    assert!(
        reads_after_jpg <= reads_after_txt + 1,
        "핸들이 닫힌 이미지 파일은 불필요한 전체 2MiB 선행 다운로드를 진행하지 않습니다"
    );

    // (3) 대용량 동영상 썸네일 (앞 16KiB ftyp 읽고, 파일 끝 64KiB moov atom 읽기)
    let h_mp4 = f.vfs.open(&p("/movie.mp4")).unwrap();
    let mut buf_16k = [0u8; 16 * 1024];
    assert_eq!(f.vfs.read(&h_mp4, 0, &mut buf_16k).unwrap(), 16 * 1024);
    // 파일 끝부분(50MiB - 64KiB)으로 탐색(Seek)
    let end_offset = 50 * 1024 * 1024 - 64 * 1024;
    assert_eq!(
        f.vfs.read(&h_mp4, end_offset, &mut buf_64k).unwrap(),
        64 * 1024
    );
    f.vfs.cleanup(&h_mp4, false).unwrap();
    f.vfs.close(&h_mp4);
    finish_prefetch(&f);
    let reads_after_mp4 = f.backend.reads.load(Ordering::SeqCst);
    assert_eq!(
        reads_after_mp4,
        reads_after_jpg + 2,
        "동영상 파일의 앞/끝 헤더 조회는 정확히 요청된 2개 블록만 읽고 전체 50MiB를 받지 않습니다"
    );

    // (4) 문서 파일 (ZIP 구조 Central Directory: 파일 끝 64KiB 읽기)
    let h_doc = f.vfs.open(&p("/document.docx")).unwrap();
    let doc_end_offset = 10 * 1024 * 1024 - 64 * 1024;
    assert_eq!(
        f.vfs.read(&h_doc, doc_end_offset, &mut buf_64k).unwrap(),
        64 * 1024
    );
    f.vfs.cleanup(&h_doc, false).unwrap();
    f.vfs.close(&h_doc);
    finish_prefetch(&f);
    let reads_after_doc = f.backend.reads.load(Ordering::SeqCst);
    assert_eq!(
        reads_after_doc,
        reads_after_mp4 + 1,
        "문서 파일 Central Directory 조회는 정확히 끝 1개 블록만 읽습니다"
    );

    // (5) Warm(캐시 예열) 상태 재조회 검증
    // 이미 메모리 캐시에 존재하는 블록 조회 시 백엔드 요청 증가가 0이어야 합니다.
    let h_txt_warm = f.vfs.open(&p("/text.txt")).unwrap();
    assert_eq!(f.vfs.read(&h_txt_warm, 0, &mut buf).unwrap(), 4096);
    f.vfs.cleanup(&h_txt_warm, false).unwrap();
    f.vfs.close(&h_txt_warm);

    let h_mp4_warm = f.vfs.open(&p("/movie.mp4")).unwrap();
    assert_eq!(f.vfs.read(&h_mp4_warm, 0, &mut buf_16k).unwrap(), 16 * 1024);
    assert_eq!(
        f.vfs.read(&h_mp4_warm, end_offset, &mut buf_64k).unwrap(),
        64 * 1024
    );
    f.vfs.cleanup(&h_mp4_warm, false).unwrap();
    f.vfs.close(&h_mp4_warm);

    let reads_warm = f.backend.reads.load(Ordering::SeqCst);
    assert_eq!(
        reads_warm, reads_after_doc,
        "예열된 캐시 상태에서는 추가적인 백엔드 네트워크 요청 없이 즉시 반환됩니다"
    );
}

#[test]
fn small_random_reads_fetch_probe_chunks_instead_of_whole_blocks() {
    let f = fixture_with(|o| {
        o.block_size = 4 << 20;
        o.readahead_blocks = 0;
    });
    let size = 10usize << 20;
    let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    std::fs::write(f.root.path().join("movie.mp4"), &data).unwrap();
    let h = f.vfs.open(&p("/movie.mp4")).unwrap();
    let chunk = PROBE_CHUNK as usize;

    // 머리 16KiB는 조각 하나, 꼬리 64KiB는 끝까지 이어지는 조각 하나만 받습니다.
    let mut head = vec![0u8; 16 * 1024];
    assert_eq!(f.vfs.read(&h, 0, &mut head).unwrap(), head.len());
    assert_eq!(head, data[..head.len()]);
    let tail_at = (size - 64 * 1024) as u64;
    let mut tail = vec![0u8; 64 * 1024];
    assert_eq!(f.vfs.read(&h, tail_at, &mut tail).unwrap(), tail.len());
    assert_eq!(tail, data[size - 64 * 1024..]);
    assert_eq!(f.backend.reads.load(Ordering::SeqCst), 2);
    assert_eq!(f.backend.read_bytes.load(Ordering::SeqCst), 2 * chunk);

    // 파일 끝 1MiB 안의 조회는 끝까지 한 번에 받아 이후 꼬리 조회는 다시 받지 않습니다.
    let near_end = (size - 900 * 1024) as u64;
    let mut piece = vec![0u8; 4096];
    assert_eq!(f.vfs.read(&h, near_end, &mut piece).unwrap(), piece.len());
    assert_eq!(piece, data[near_end as usize..near_end as usize + 4096]);
    assert_eq!(f.backend.reads.load(Ordering::SeqCst), 3);
    let mid_tail = (size - 300 * 1024) as u64;
    assert_eq!(f.vfs.read(&h, mid_tail, &mut piece).unwrap(), piece.len());
    assert_eq!(piece, data[mid_tail as usize..mid_tail as usize + 4096]);
    assert_eq!(f.backend.reads.load(Ordering::SeqCst), 3);
    let reads_before_cross = f.backend.reads.load(Ordering::SeqCst);

    // 같은 조각 안의 다른 위치는 다시 받지 않습니다.
    let mut again = vec![0u8; 4096];
    assert_eq!(f.vfs.read(&h, 100_000, &mut again).unwrap(), again.len());
    assert_eq!(again, data[100_000..104_096]);
    assert_eq!(f.backend.reads.load(Ordering::SeqCst), reads_before_cross);

    // 조각 경계에 걸친 읽기는 이어지는 조각을 하나 더 받습니다.
    let at = PROBE_CHUNK - 1000;
    let mut cross = vec![0u8; 8192];
    assert_eq!(f.vfs.read(&h, at, &mut cross).unwrap(), cross.len());
    assert_eq!(cross, data[at as usize..at as usize + 8192]);
    assert_eq!(
        f.backend.reads.load(Ordering::SeqCst),
        reads_before_cross + 1
    );

    // 연속 읽기가 1MiB를 넘으면 일반 블록 읽기로 전환하고, 이후 짧은 읽기도 그 블록을 씁니다.
    let mut pos = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    while pos < (2 << 20) {
        let n = f.vfs.read(&h, pos, &mut buf).unwrap();
        assert_eq!(buf[..n], data[pos as usize..pos as usize + n]);
        pos += n as u64;
    }
    let bytes = f.backend.read_bytes.load(Ordering::SeqCst);
    assert!(bytes >= 4 << 20, "첫 블록 전체를 받았습니다: {bytes}");
    let reads = f.backend.reads.load(Ordering::SeqCst);
    assert_eq!(f.vfs.read(&h, 3 << 20, &mut again).unwrap(), again.len());
    assert_eq!(f.backend.reads.load(Ordering::SeqCst), reads);
}

#[test]
fn sequential_transfer_backends_prefetch_blocks_in_order() {
    let f = fixture_with(|o| {
        o.block_size = 64 * 1024;
        o.readahead_blocks = 4;
    });
    f.backend.sequential.store(true, Ordering::SeqCst);
    *f.backend.read_delay.lock() = Duration::from_millis(15);
    let data: Vec<u8> = (0..(40 * 64 * 1024)).map(|i| (i % 249) as u8).collect();
    std::fs::write(f.root.path().join("doc.pdf"), &data).unwrap();
    let h = f.vfs.open(&p("/doc.pdf")).unwrap();
    // PowerToys PDF 썸네일처럼 처음부터 끝까지 큰 단위로 이어 읽습니다.
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.vfs.read(&h, out.len() as u64, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    assert_eq!(out, data);
    finish_prefetch(&f);
    let offsets = f.backend.read_offsets.lock().clone();
    let mut unique = offsets.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        offsets.len(),
        "블록마다 한 번만 받습니다: {offsets:?}"
    );
    assert_eq!(offsets.len(), 40);
    assert!(
        f.backend.max_reading.load(Ordering::SeqCst) <= 2,
        "선행 읽기는 한 번에 하나씩: 최대 {}",
        f.backend.max_reading.load(Ordering::SeqCst)
    );
    let behind = offsets.windows(2).filter(|w| w[1] < w[0]).count();
    assert!(
        behind <= 1,
        "요청 순서가 거의 오름차순이어야 합니다: {offsets:?}"
    );
}

/// 탐색기 복사(생성, 크기 설정, 1 MiB 순차 쓰기, 수정 시각, 정리)를 흉내 내어
/// 호출 프로그램이 완료를 보는 시점과 원격에 안착하는 시점을 잽니다.
fn copy_files(f: &Fixture, files: &[(String, usize)]) -> (f64, f64) {
    let started = Instant::now();
    for (name, size) in files {
        let path = p(&format!("/{name}"));
        let h = f.vfs.create(&path, EntryKind::File).unwrap();
        f.vfs.set_size(&h, *size as u64, false).unwrap();
        let chunk = vec![7u8; 1 << 20];
        let mut off = 0usize;
        while off < *size {
            let n = (*size - off).min(chunk.len());
            f.vfs
                .write(&h, off as u64, &chunk[..n], false, false)
                .unwrap();
            off += n;
        }
        f.vfs
            .set_mtime(&h, UNIX_EPOCH + Duration::from_secs(1_700_000_000))
            .unwrap();
        f.vfs.cleanup(&h, false).unwrap();
        f.vfs.close(&h);
    }
    let visible = started.elapsed().as_secs_f64() * 1000.0;
    f.vfs.sync(Duration::from_secs(120)).unwrap();
    let landed = started.elapsed().as_secs_f64() * 1000.0;
    for (name, size) in files {
        assert_eq!(
            std::fs::metadata(f.root.path().join(name)).unwrap().len(),
            *size as u64,
            "{name}"
        );
    }
    (visible, landed)
}

/// 왕복 20ms, 업로드 64 MiB/s 망에서 백그라운드 업로드와 실시간 업로드를 비교합니다.
/// 백엔드 호출의 왕복 수는 SFTP를 따릅니다(조회 1회, 전체 쓰기 5회, 세션 열기 1~2회와 확정 3회).
/// - 큰 파일 하나(48 MiB)와 작은 파일 여러 개(16 KiB x 40)
/// - visible: 호출 프로그램이 완료를 보는 시간, landed: 원격에 모두 안착한 시간(각 3회 중앙값)
#[test]
#[ignore = "통제 쓰기 성능 비교: cargo test -p expandrv-vfs write_mode_comparison --release -- --ignored --nocapture"]
fn write_mode_comparison() {
    let scenarios: [(&str, Vec<(String, usize)>); 2] = [
        ("big", vec![("big.bin".to_owned(), 48 << 20)]),
        (
            "many",
            (0..40)
                .map(|i| (format!("small-{i}.txt"), 16 << 10))
                .collect(),
        ),
    ];
    // Windows 타이머 흔들림을 줄이려고 두 모드를 번갈아 3회 재고 중앙값을 씁니다.
    const ROUNDS: usize = 3;
    let mut out = Vec::new();
    for (tag, files) in &scenarios {
        let mut samples: [Vec<(f64, f64)>; 2] = [Vec::new(), Vec::new()];
        for _ in 0..ROUNDS {
            for (i, mode) in [WriteMode::Back, WriteMode::Through]
                .into_iter()
                .enumerate()
            {
                let f = fixture_with(|o| o.write_mode = mode);
                *f.backend.net_rtt.lock() = Duration::from_millis(20);
                *f.backend.metadata_delay.lock() = Duration::from_millis(20);
                f.backend.net_bps.store(64 << 20, Ordering::SeqCst);
                samples[i].push(copy_files(&f, files));
            }
        }
        for (i, name) in ["back", "through"].into_iter().enumerate() {
            let mut visible: Vec<f64> = samples[i].iter().map(|s| s.0).collect();
            let mut landed: Vec<f64> = samples[i].iter().map(|s| s.1).collect();
            visible.sort_by(f64::total_cmp);
            landed.sort_by(f64::total_cmp);
            out.push((
                format!("{tag}_{name}"),
                visible[ROUNDS / 2],
                landed[ROUNDS / 2],
            ));
        }
    }
    let json: Vec<String> = out
        .iter()
        .map(|(k, v, l)| {
            format!(
                "\"{k}_visible_ms\":{v:.1},\"{k}_landed_ms\":{l:.1},\"{k}_gap_ms\":{:.1}",
                l - v
            )
        })
        .collect();
    println!("PERFORMANCE_WRITE {{{}}}", json.join(","));
    for pair in out.chunks(2) {
        let ((_, _, back_landed), (key, through_visible, through_landed)) = (&pair[0], &pair[1]);
        // 실시간 모드는 완료가 보이는 시점에 이미 원격에 안착해 있습니다.
        assert!(
            through_landed - through_visible < 50.0,
            "{key}: 완료 표시 뒤 남은 업로드 {:.1}ms",
            through_landed - through_visible
        );
        // 원격 안착까지의 총 시간은 백그라운드 모드보다 늘지 않습니다(10% 허용).
        assert!(
            *through_landed <= back_landed * 1.1,
            "{key}: 실시간 {through_landed:.1}ms, 백그라운드 {back_landed:.1}ms"
        );
    }
}
