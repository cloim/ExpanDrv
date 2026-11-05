//! 실제 WebDAV 서버 통합 테스트. `EXPANDRV_TEST_WEBDAV_URL`이 없으면 건너뜁니다.
//!
//! 예: `docker run -d --rm --name expandrv-test-webdav -p 18080:8080 rclone/rclone serve webdav /data --addr :8080 --user u --pass p`
//! 후 `EXPANDRV_TEST_WEBDAV_URL=http://127.0.0.1:18080/`, `EXPANDRV_TEST_WEBDAV_USER=u`, `EXPANDRV_TEST_WEBDAV_PASS=p`
//!
//! `EXPANDRV_TEST_WEBDAV_TLS_URL`과 `EXPANDRV_TEST_WEBDAV_PIN`(서버 인증서 SHA-256)을 주면 인증서 고정도 확인합니다.
#![cfg(feature = "webdav")]

use bytes::Bytes;
use expandrv_backends::webdav::WebDavBackend;
use expandrv_core::config::{SecretResolver, SecretSource, WebDavConfig};
use expandrv_core::{Backend, EntryKind, Error, RemotePath, Result, WriteSource};
use secrecy::SecretString;
use std::time::{SystemTime, UNIX_EPOCH};

struct Pass(String);

impl SecretResolver for Pass {
    fn resolve(&self, _: &SecretSource) -> Result<SecretString> {
        Ok(SecretString::from(self.0.clone()))
    }
}

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn config(url: String, pin: Option<String>) -> WebDavConfig {
    WebDavConfig {
        url,
        username: Some(env("EXPANDRV_TEST_WEBDAV_USER", "u")),
        password: Some(SecretSource::Env {
            var: "EXPANDRV_TEST_WEBDAV_PASS".into(),
        }),
        pinned_cert_sha256: pin,
    }
}

fn secrets() -> Pass {
    Pass(env("EXPANDRV_TEST_WEBDAV_PASS", "p"))
}

fn p(s: &str) -> RemotePath {
    RemotePath::parse(s).unwrap()
}

fn base_url() -> Option<String> {
    let url = std::env::var("EXPANDRV_TEST_WEBDAV_URL").ok();
    if url.is_none() {
        eprintln!("EXPANDRV_TEST_WEBDAV_URL이 없어 WebDAV 통합 테스트를 건너뜁니다");
    }
    url
}

/// 실행마다 겹치지 않는 작업 루트
fn unique_root(tag: &str) -> RemotePath {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p(&format!("/it-{tag}-{n:x}"))
}

#[tokio::test]
async fn webdav_roundtrip() {
    let Some(url) = base_url() else { return };
    let be = WebDavBackend::connect(&config(url, None), &secrets())
        .await
        .unwrap();
    let caps = be.capabilities();
    assert!(caps.range_read && caps.rename_dir && !caps.set_mtime);

    let root = unique_root("rt");
    let d = be.mkdir(&root).await.unwrap();
    assert!(d.is_dir());
    assert!(matches!(be.mkdir(&root).await, Err(Error::AlreadyExists)));

    // 유니코드와 공백, 예약 문자가 섞인 이름
    let sub = root.join("한글 폴더 #1").unwrap();
    be.mkdir(&sub).await.unwrap();
    let small = sub.join("a b+c%d.txt").unwrap();
    let e = be
        .write(
            &small,
            WriteSource::Bytes(Bytes::from_static(b"hello webdav world")),
        )
        .await
        .unwrap();
    assert_eq!(
        (e.name.as_str(), e.size, e.kind),
        ("a b+c%d.txt", 18, EntryKind::File)
    );

    // 1 MiB 넘는 파일을 스트리밍 업로드
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big.bin");
    let big: Vec<u8> = (0..(3 << 20) + 12345).map(|i| (i % 251) as u8).collect();
    std::fs::write(&src, &big).unwrap();
    let big_path = root.join("big.bin").unwrap();
    let e = be
        .write(
            &big_path,
            WriteSource::File {
                path: src,
                size: big.len() as u64,
            },
        )
        .await
        .unwrap();
    assert_eq!(e.size, big.len() as u64);

    // stat
    let st = be.stat(&big_path).await.unwrap();
    assert_eq!((st.name.as_str(), st.size), ("big.bin", big.len() as u64));
    assert!(st.modified.is_some());
    let st = be.stat(&sub).await.unwrap();
    assert!(st.is_dir());
    assert_eq!(st.name, "한글 폴더 #1");
    assert!(be.stat(&RemotePath::root()).await.unwrap().is_dir());

    // list
    let mut names: Vec<_> = be
        .list(&root)
        .await
        .unwrap()
        .into_iter()
        .map(|e| (e.name, e.kind))
        .collect();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        names,
        [
            ("big.bin".to_owned(), EntryKind::File),
            ("한글 폴더 #1".to_owned(), EntryKind::Dir)
        ]
    );
    let inner = be.list(&sub).await.unwrap();
    assert_eq!(inner.len(), 1);
    assert_eq!((inner[0].name.as_str(), inner[0].size), ("a b+c%d.txt", 18));
    assert!(matches!(be.list(&small).await, Err(Error::NotADirectory)));

    // read_range: 중간, EOF 걸침, EOF 이후
    let mid = be.read_range(&big_path, 1_000_000, 4096).await.unwrap();
    assert_eq!(&mid[..], &big[1_000_000..1_004_096]);
    let tail = be
        .read_range(&big_path, big.len() as u64 - 10, 100)
        .await
        .unwrap();
    assert_eq!(&tail[..], &big[big.len() - 10..]);
    assert!(
        be.read_range(&big_path, big.len() as u64 + 5, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(&be.read_range(&small, 6, 6).await.unwrap()[..], b"webdav");

    // rename: 대체 없음(대상 있음), 대체 있음, 새 이름, 디렉터리
    let other = root.join("other.txt").unwrap();
    be.write(&other, WriteSource::Bytes(Bytes::from_static(b"OTHER")))
        .await
        .unwrap();
    assert!(matches!(
        be.rename(&small, &other, false).await,
        Err(Error::AlreadyExists)
    ));
    assert_eq!(&be.read_range(&other, 0, 100).await.unwrap()[..], b"OTHER");
    be.rename(&small, &other, true).await.unwrap();
    assert_eq!(
        &be.read_range(&other, 0, 100).await.unwrap()[..],
        b"hello webdav world"
    );
    assert!(matches!(be.stat(&small).await, Err(Error::NotFound)));
    let renamed = root.join("renamed.txt").unwrap();
    be.rename(&other, &renamed, false).await.unwrap();
    assert!(matches!(be.stat(&other).await, Err(Error::NotFound)));
    let moved_dir = root.join("moved dir").unwrap();
    be.rename(&sub, &moved_dir, false).await.unwrap();
    assert!(be.stat(&moved_dir).await.unwrap().is_dir());
    assert!(matches!(
        be.rename(&renamed, &moved_dir, true).await,
        Err(Error::IsADirectory)
    ));

    // NotFound
    let missing = root.join("missing").unwrap();
    assert!(matches!(be.stat(&missing).await, Err(Error::NotFound)));
    assert!(matches!(be.list(&missing).await, Err(Error::NotFound)));
    assert!(matches!(
        be.read_range(&missing, 0, 10).await,
        Err(Error::NotFound)
    ));
    assert!(matches!(
        be.delete(&missing, EntryKind::File).await,
        Err(Error::NotFound)
    ));
    assert!(matches!(
        be.rename(&missing, &root.join("x").unwrap(), false).await,
        Err(Error::NotFound)
    ));
    assert!(matches!(
        be.write(
            &missing.join("child.txt").unwrap(),
            WriteSource::Bytes(Bytes::from_static(b"x"))
        )
        .await,
        Err(Error::NotFound)
    ));

    // delete: 파일, 비어 있지 않은 디렉터리 거부, 종류 불일치, 빈 디렉터리
    assert!(matches!(
        be.delete(&root, EntryKind::Dir).await,
        Err(Error::DirectoryNotEmpty)
    ));
    assert!(matches!(
        be.delete(&moved_dir, EntryKind::File).await,
        Err(Error::IsADirectory)
    ));
    assert!(matches!(
        be.delete(&renamed, EntryKind::Dir).await,
        Err(Error::NotADirectory)
    ));
    be.delete(&renamed, EntryKind::File).await.unwrap();
    be.delete(&big_path, EntryKind::File).await.unwrap();
    be.delete(&moved_dir, EntryKind::Dir).await.unwrap();
    assert!(be.list(&root).await.unwrap().is_empty());
    be.delete(&root, EntryKind::Dir).await.unwrap();
    assert!(matches!(be.stat(&root).await, Err(Error::NotFound)));

    // 할당량은 서버가 모르면 기본값입니다.
    be.quota().await.unwrap();
}

#[tokio::test]
async fn webdav_rejects_wrong_password() {
    let Some(url) = base_url() else { return };
    let r = WebDavBackend::connect(&config(url, None), &Pass("wrong-password".into())).await;
    match r {
        Err(Error::Auth(msg)) => assert!(!msg.contains("wrong-password")),
        Err(e) => panic!("Auth 오류를 기대했습니다: {e:?}"),
        Ok(_) => panic!("잘못된 비밀번호로 연결되었습니다"),
    }
}

#[tokio::test]
async fn webdav_pinned_certificate() {
    let (Ok(url), Ok(pin)) = (
        std::env::var("EXPANDRV_TEST_WEBDAV_TLS_URL"),
        std::env::var("EXPANDRV_TEST_WEBDAV_PIN"),
    ) else {
        eprintln!("EXPANDRV_TEST_WEBDAV_TLS_URL/PIN이 없어 인증서 고정 테스트를 건너뜁니다");
        return;
    };
    // 자체 서명 인증서는 일반 검증으로는 거부됩니다.
    assert!(matches!(
        WebDavBackend::connect(&config(url.clone(), None), &secrets()).await,
        Err(Error::Network(_))
    ));
    let be = WebDavBackend::connect(&config(url.clone(), Some(pin)), &secrets())
        .await
        .unwrap();
    assert!(be.stat(&RemotePath::root()).await.unwrap().is_dir());
    let wrong = "00".repeat(32);
    assert!(matches!(
        WebDavBackend::connect(&config(url, Some(wrong)), &secrets()).await,
        Err(Error::Network(_))
    ));
}
