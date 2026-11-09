//! FTP/FTPS 통합 테스트. 실제 서버가 필요하므로 환경 변수가 없으면 건너뜁니다.
//!
//! - `EXPANDRV_TEST_FTP_HOST`, `EXPANDRV_TEST_FTP_PORT`: 평문 FTP 서버
//! - `EXPANDRV_TEST_FTP_USER`, `EXPANDRV_TEST_FTP_PASS`: 계정(기본 `u`), 비밀번호
//! - `EXPANDRV_TEST_FTP_ROOT`: 원격 루트(기본 `.` = 로그인 디렉터리)
//! - `EXPANDRV_TEST_FTPS_PORT`, `EXPANDRV_TEST_FTPS_CA`: 명시적 FTPS 서버와 신뢰할 CA 인증서(PEM)
//!
//! 예시(vsftpd):
//! `docker run -d --rm --name expandrv-test-ftp -p 12121:21 -p 30000-30009:30000-30009
//!  -e USERS="u|p" -e ADDRESS=127.0.0.1 -e MIN_PORT=30000 -e MAX_PORT=30009 delfer/alpine-ftp-server`
#![cfg(feature = "ftp")]

use bytes::Bytes;
use expandrv_backends::ftp::FtpBackend;
use expandrv_core::config::{EnvSecrets, FtpConfig, FtpTls, SecretResolver, SecretSource};
use expandrv_core::{Backend, EntryKind, Error, RemotePath, WriteSource};
use secrecy::SecretString;
use std::sync::Arc;
use suppaftp::tokio_rustls::rustls::pki_types::CertificateDer;
use suppaftp::tokio_rustls::rustls::pki_types::pem::PemObject;
use suppaftp::tokio_rustls::rustls::{self, ClientConfig, RootCertStore};

/// 실패 시 실제 결과를 함께 보여 주는 오류 단언
macro_rules! assert_err {
    ($e:expr, $p:pat) => {{
        let r = $e;
        assert!(
            matches!(r, Err($p)),
            "{} => {:?}",
            stringify!($e),
            r.map(|_| ())
        );
    }};
}

fn p(s: &str) -> RemotePath {
    RemotePath::parse(s).unwrap()
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn config(port_var: &str, tls: FtpTls, encoding: &str) -> Option<FtpConfig> {
    let (Some(host), Some(port)) = (env("EXPANDRV_TEST_FTP_HOST"), env(port_var)) else {
        eprintln!("EXPANDRV_TEST_FTP_HOST/{port_var}가 없어 FTP 통합 테스트를 건너뜁니다");
        return None;
    };
    Some(FtpConfig {
        pinned_cert_sha256: None,
        host,
        port: port.parse().unwrap(),
        username: env("EXPANDRV_TEST_FTP_USER").unwrap_or_else(|| "u".into()),
        password: Some(SecretSource::Env {
            var: "EXPANDRV_TEST_FTP_PASS".into(),
        }),
        tls,
        encoding: encoding.into(),
        root: env("EXPANDRV_TEST_FTP_ROOT").unwrap_or_else(|| ".".into()),
    })
}

/// 테스트끼리 겹치지 않는 작업 디렉터리 이름
fn unique(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("/expandrv-{tag}-{}-{nanos:x}", std::process::id())
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

/// 공통 시나리오: mkdir, 바이트/파일 쓰기, stat, list, 부분 읽기, 이름 변경, 삭제, NotFound
async fn exercise(be: Arc<FtpBackend>, base: &str) {
    let dir = p(base);
    let e = be.mkdir(&dir).await.unwrap();
    assert_eq!(e.kind, EntryKind::Dir);
    assert_err!(be.mkdir(&dir).await, Error::AlreadyExists);

    let a = dir.join("a.txt").unwrap();
    let e = be
        .write(&a, WriteSource::Bytes(Bytes::from_static(b"hello world")))
        .await
        .unwrap();
    assert_eq!(
        (e.name.as_str(), e.size, e.kind),
        ("a.txt", 11, EntryKind::File)
    );

    // 1 MiB 넘는 로컬 파일 스트리밍 업로드
    let data = pattern(1_572_864 + 123);
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), &data).unwrap();
    let big = dir.join("big file.bin").unwrap();
    let e = be
        .write(
            &big,
            WriteSource::File {
                path: tmp.path().to_owned(),
                size: data.len() as u64,
            },
        )
        .await
        .unwrap();
    assert_eq!(e.size, data.len() as u64);
    assert_eq!(be.stat(&big).await.unwrap().size, data.len() as u64);

    let mut names: Vec<_> = be
        .list(&dir)
        .await
        .unwrap()
        .into_iter()
        .map(|e| (e.name, e.kind, e.size))
        .collect();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        names,
        [
            ("a.txt".to_owned(), EntryKind::File, 11),
            (
                "big file.bin".to_owned(),
                EntryKind::File,
                data.len() as u64
            ),
        ]
    );

    // 중간 부분 읽기 뒤 같은 풀에서 다른 작업이 이어져야 합니다(중단한 전송 정리 확인).
    for round in 0..5u64 {
        let off = 100_000 + round * 200_000;
        let got = be.read_range(&big, off, 65_536).await.unwrap();
        assert_eq!(
            &got[..],
            &data[off as usize..off as usize + 65_536],
            "round {round}"
        );
        assert_eq!(be.stat(&a).await.unwrap().size, 11);
    }
    // EOF에 걸친 읽기, EOF 이후 읽기, 끝까지 정확히 읽기
    let n = data.len() as u64;
    assert_eq!(
        &be.read_range(&big, n - 5, 100).await.unwrap()[..],
        &data[data.len() - 5..]
    );
    assert!(be.read_range(&big, n + 10, 100).await.unwrap().is_empty());
    assert!(be.read_range(&big, n, 1).await.unwrap().is_empty());
    assert_eq!(&be.read_range(&a, 6, 5).await.unwrap()[..], b"world");
    assert_eq!(&be.read_range(&a, 0, 11).await.unwrap()[..], b"hello world");
    assert_eq!(be.read_range(&big, 0, n).await.unwrap().len() as u64, n);

    // 1MiB 이상 연속 읽기는 같은 전송을 이어 받습니다. 경계 바이트와 EOF 정리를 확인합니다.
    let first = be.read_range(&big, 0, 1 << 20).await.unwrap();
    assert_eq!(&first[..], &data[..1 << 20]);
    let rest = be.read_range(&big, 1 << 20, 1 << 20).await.unwrap();
    assert_eq!(&rest[..], &data[1 << 20..], "이어 받은 나머지와 EOF");
    assert_eq!(be.stat(&a).await.unwrap().size, 11, "EOF 뒤 연결 재사용");
    // 이어 받기를 기다리는 동안 파일이 바뀌면 이전 내용을 돌려주지 않습니다.
    let first = be.read_range(&big, 0, 1 << 20).await.unwrap();
    assert_eq!(&first[..], &data[..1 << 20]);
    let changed: Vec<u8> = data.iter().map(|b| b.wrapping_add(1)).collect();
    be.write(&big, WriteSource::Bytes(Bytes::from(changed.clone())))
        .await
        .unwrap();
    let after = be.read_range(&big, 1 << 20, 4096).await.unwrap();
    assert_eq!(&after[..], &changed[1 << 20..(1 << 20) + 4096]);
    be.write(&big, WriteSource::Bytes(Bytes::from(data.clone())))
        .await
        .unwrap();
    // 이어지지 않는 위치를 읽으면 대기 전송과 별개로 새 전송을 씁니다.
    let first = be.read_range(&big, 0, 1 << 20).await.unwrap();
    assert_eq!(&first[..], &data[..1 << 20]);
    let other = be.read_range(&big, 1_100_000, 10_000).await.unwrap();
    assert_eq!(&other[..], &data[1_100_000..1_110_000]);
    let rest = be.read_range(&big, 1 << 20, 1 << 20).await.unwrap();
    assert_eq!(&rest[..], &data[1 << 20..]);

    // 풀 동시 사용
    let mut tasks = Vec::new();
    for i in 0..8u64 {
        let (be, big, expect) = (Arc::clone(&be), big.clone(), data.clone());
        tasks.push(tokio::spawn(async move {
            let off = i * 150_000;
            let got = be.read_range(&big, off, 10_000).await.unwrap();
            assert_eq!(&got[..], &expect[off as usize..off as usize + 10_000]);
            be.stat(&big).await.unwrap();
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }

    // 이름 변경
    let b = dir.join("b.txt").unwrap();
    be.rename(&a, &b, false).await.unwrap();
    assert_err!(be.stat(&a).await, Error::NotFound);
    let c = dir.join("c.txt").unwrap();
    be.write(&c, WriteSource::Bytes(Bytes::from_static(b"CCC")))
        .await
        .unwrap();
    assert_err!(be.rename(&c, &b, false).await, Error::AlreadyExists);
    be.rename(&c, &b, true).await.unwrap();
    assert_eq!(&be.read_range(&b, 0, 10).await.unwrap()[..], b"CCC");
    assert_err!(be.rename(&c, &b, true).await, Error::NotFound);

    // 디렉터리 이름 변경
    let sub = dir.join("sub").unwrap();
    be.mkdir(&sub).await.unwrap();
    let sub2 = dir.join("sub moved").unwrap();
    be.rename(&sub, &sub2, false).await.unwrap();
    assert!(be.stat(&sub2).await.unwrap().is_dir());

    // NotFound, NotADirectory, IsADirectory
    let missing = dir.join("missing").unwrap();
    assert_err!(be.stat(&missing).await, Error::NotFound);
    assert_err!(be.list(&missing).await, Error::NotFound);
    assert_err!(be.read_range(&missing, 0, 10).await, Error::NotFound);
    assert_err!(be.list(&b).await, Error::NotADirectory);
    assert_err!(be.read_range(&sub2, 0, 10).await, Error::IsADirectory);
    assert_err!(be.delete(&missing, EntryKind::File).await, Error::NotFound);

    // 삭제
    assert_err!(
        be.delete(&dir, EntryKind::Dir).await,
        Error::DirectoryNotEmpty
    );
    be.delete(&b, EntryKind::File).await.unwrap();
    be.delete(&big, EntryKind::File).await.unwrap();
    be.delete(&sub2, EntryKind::Dir).await.unwrap();
    assert!(be.list(&dir).await.unwrap().is_empty());
    be.delete(&dir, EntryKind::Dir).await.unwrap();
    assert_err!(be.stat(&dir).await, Error::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plain_ftp_roundtrip() {
    let Some(cfg) = config("EXPANDRV_TEST_FTP_PORT", FtpTls::None, "utf-8") else {
        return;
    };
    let be = Arc::new(FtpBackend::connect(&cfg, &EnvSecrets).await.unwrap());
    let caps = be.capabilities();
    eprintln!("FTP capabilities: {caps:?}");
    assert!(caps.range_read && caps.rename_dir && caps.case_sensitive);
    assert!(be.stat(&RemotePath::root()).await.unwrap().is_dir());
    if caps.set_mtime {
        // MFMT를 지원하는 서버(pure-ftpd 등)
        let f = p(&unique("mtime"));
        be.write(&f, WriteSource::Bytes(Bytes::from_static(b"t")))
            .await
            .unwrap();
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        be.set_mtime(&f, t).await.unwrap();
        assert_eq!(be.stat(&f).await.unwrap().modified, Some(t));
        be.delete(&f, EntryKind::File).await.unwrap();
    } else {
        assert_err!(
            be.set_mtime(&p("/x"), std::time::SystemTime::now()).await,
            Error::Unsupported(_)
        );
    }
    exercise(be, &unique("plain")).await;
}

#[tokio::test]
async fn legacy_charset_names() {
    let Some(cfg) = config("EXPANDRV_TEST_FTP_PORT", FtpTls::None, "euc-kr") else {
        return;
    };
    let euckr = FtpBackend::connect(&cfg, &EnvSecrets).await.unwrap();
    let utf8_cfg = FtpConfig {
        encoding: "utf-8".into(),
        ..cfg
    };
    let utf8 = FtpBackend::connect(&utf8_cfg, &EnvSecrets).await.unwrap();

    let dir = p(&unique("charset"));
    euckr.mkdir(&dir).await.unwrap();
    let f = dir.join("한글 파일.txt").unwrap();
    euckr
        .write(&f, WriteSource::Bytes(Bytes::from_static(b"kr")))
        .await
        .unwrap();
    let names = |v: Vec<expandrv_core::Entry>| v.into_iter().map(|e| e.name).collect::<Vec<_>>();
    assert_eq!(names(euckr.list(&dir).await.unwrap()), ["한글 파일.txt"]);
    // 같은 바이트를 UTF-8로 읽으면 다른 이름이 됩니다.
    let raw = names(utf8.list(&dir).await.unwrap());
    assert_eq!(raw.len(), 1);
    assert_ne!(raw[0], "한글 파일.txt");
    assert_eq!(&euckr.read_range(&f, 0, 10).await.unwrap()[..], b"kr");

    // UTF-8 이름도 그대로 왕복합니다.
    let g = dir.join("유니코드 ✓.txt").unwrap();
    utf8.write(&g, WriteSource::Bytes(Bytes::from_static(b"u")))
        .await
        .unwrap();
    assert!(names(utf8.list(&dir).await.unwrap()).contains(&"유니코드 ✓.txt".to_owned()));
    utf8.delete(&g, EntryKind::File).await.unwrap();

    euckr.delete(&f, EntryKind::File).await.unwrap();
    euckr.delete(&dir, EntryKind::Dir).await.unwrap();
}

/// 항상 같은 값을 돌려주는 해석기
struct Fixed(&'static str);

impl SecretResolver for Fixed {
    fn resolve(&self, _: &SecretSource) -> expandrv_core::Result<SecretString> {
        Ok(SecretString::from(self.0))
    }
}

#[tokio::test]
async fn wrong_password_is_auth_error() {
    let Some(cfg) = config("EXPANDRV_TEST_FTP_PORT", FtpTls::None, "utf-8") else {
        return;
    };
    match FtpBackend::connect(&cfg, &Fixed("definitely-wrong")).await {
        Err(Error::Auth(_)) => {}
        Err(e) => panic!("Auth 오류가 아님: {e}"),
        Ok(_) => panic!("잘못된 비밀번호로 로그인됨"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn explicit_ftps_roundtrip() {
    let Some(cfg) = config("EXPANDRV_TEST_FTPS_PORT", FtpTls::Explicit, "utf-8") else {
        return;
    };
    let Some(ca) = env("EXPANDRV_TEST_FTPS_CA") else {
        eprintln!("EXPANDRV_TEST_FTPS_CA가 없어 FTPS 통합 테스트를 건너뜁니다");
        return;
    };
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_file(&ca).unwrap())
        .unwrap();
    let tls =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    let be = FtpBackend::connect_with_tls(&cfg, &EnvSecrets, Arc::new(tls))
        .await
        .unwrap();
    exercise(Arc::new(be), &unique("ftps")).await;

    // 시스템 저장소에 없는 자체 서명 CA는 기본 검증에서 거부됩니다.
    assert_err!(
        FtpBackend::connect(&cfg, &EnvSecrets).await,
        Error::Network(_)
    );
}
