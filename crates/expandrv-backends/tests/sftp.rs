//! 실제 SFTP 서버 통합 테스트. 환경 변수가 없으면 건너뜁니다.
//!
//! - `EXPANDRV_TEST_SFTP_HOST`, `_PORT`, `_USER`, `_PASSWORD`, `_KNOWN_HOSTS`, `_ROOT`(기본 `/data`)
//! - 키 로그인: `_KEY_USER`, `_KEY`(OpenSSH 개인 키 경로), `_KEY_PASSPHRASE`(선택)
//! - 재연결: `_CONTAINER`(테스트 중 `docker restart`할 컨테이너 이름). 다른 테스트를 끊으므로 `--test-threads=1`로 실행합니다.
#![cfg(feature = "sftp")]

use bytes::Bytes;
use expandrv_backends::sftp::SftpBackend;
use expandrv_core::config::{BackendConfig, EnvSecrets, SecretResolver, SecretSource, SftpConfig};
use expandrv_core::{Backend, EntryKind, Error, RemotePath, WriteSource};
use secrecy::SecretString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PW_VAR: &str = "EXPANDRV_TEST_SFTP_PASSWORD";

fn ssh_alias_config() -> Option<SftpConfig> {
    let alias = std::env::var("EXPANDRV_TEST_SSH_ALIAS").ok()?;
    Some(SftpConfig {
        ssh_shell: std::env::var_os("EXPANDRV_TEST_SSH_SHELL").is_some(),
        use_ssh_config: true,
        host: alias,
        port: 22,
        username: String::new(),
        password: None,
        private_key: None,
        key_passphrase: None,
        root: env("ROOT").unwrap_or_else(|| "/".into()),
        known_hosts: None,
    })
}

/// 로컬 SSH 별칭으로 접속해 목록만 조회합니다. 원격 파일은 변경하지 않습니다.
#[tokio::test]
async fn ssh_alias_lists_root_read_only() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let Some(cfg) = ssh_alias_config() else {
        return;
    };
    let be = tokio::time::timeout(
        Duration::from_secs(30),
        SftpBackend::connect(&cfg, &expandrv_core::config::EnvSecrets),
    )
    .await
    .expect("SSH/SFTP 연결 준비 30초 초과")
    .unwrap();
    let entries = be.list(&RemotePath::parse("/").unwrap()).await.unwrap();
    eprintln!("SSH 별칭 연결 및 루트 조회 성공: {}개 항목", entries.len());
}

fn env(name: &str) -> Option<String> {
    std::env::var(format!("EXPANDRV_TEST_SFTP_{name}"))
        .ok()
        .filter(|v| !v.is_empty())
}

fn base_config() -> Option<SftpConfig> {
    if std::env::var_os("EXPANDRV_TEST_SSH_SHELL").is_some() {
        return ssh_alias_config();
    }
    let (Some(host), Some(user), Some(kh)) = (env("HOST"), env("USER"), env("KNOWN_HOSTS")) else {
        eprintln!("EXPANDRV_TEST_SFTP_HOST/USER/KNOWN_HOSTS가 없어 SFTP 통합 테스트를 건너뜁니다");
        return None;
    };
    Some(SftpConfig {
        ssh_shell: false,
        use_ssh_config: false,
        host,
        port: env("PORT").map_or(22, |p| p.parse().unwrap()),
        username: user,
        password: env("PASSWORD").map(|_| SecretSource::Env { var: PW_VAR.into() }),
        private_key: None,
        key_passphrase: None,
        root: env("ROOT").unwrap_or_else(|| "/data".into()),
        known_hosts: Some(PathBuf::from(kh)),
    })
}

fn p(s: &str) -> RemotePath {
    RemotePath::parse(s).unwrap()
}

fn unique(prefix: &str) -> String {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{n:x}")
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sftp_crud_against_real_server() {
    let Some(cfg) = base_config() else { return };
    let be = expandrv_backends::connect(&BackendConfig::Sftp(cfg), None, &EnvSecrets)
        .await
        .unwrap();
    assert_eq!(be.kind(), "sftp");
    assert!(be.capabilities().case_sensitive && be.capabilities().set_mtime);

    let base = p(&format!("/{}", unique("it")));
    let d = be.mkdir(&base).await.unwrap();
    assert!(d.is_dir());
    assert!(matches!(be.mkdir(&base).await, Err(Error::AlreadyExists)));
    assert!(matches!(
        be.mkdir(&p(&format!("{base}/no/such"))).await,
        Err(Error::NotFound)
    ));
    // 경로를 셸 명령으로 보간하지 않고, 한글·따옴표·셸 특수문자를 보존합니다.
    let special = base.join("한글 ' $() & 파일.txt").unwrap();
    be.write(
        &special,
        WriteSource::Bytes(Bytes::from_static(b"literal filename")),
    )
    .await
    .unwrap();
    assert_eq!(
        &be.read_range(&special, 0, 32).await.unwrap()[..],
        b"literal filename"
    );
    be.delete(&special, EntryKind::File).await.unwrap();

    // 바이트 쓰기와 부분 읽기
    let a = base.join("a.txt").unwrap();
    let e = be
        .write(&a, WriteSource::Bytes(Bytes::from_static(b"hello world")))
        .await
        .unwrap();
    assert_eq!(
        (e.name.as_str(), e.size, e.kind),
        ("a.txt", 11, EntryKind::File)
    );
    assert_eq!(&be.read_range(&a, 6, 5).await.unwrap()[..], b"world");
    assert_eq!(&be.read_range(&a, 6, 100).await.unwrap()[..], b"world");
    assert!(be.read_range(&a, 11, 10).await.unwrap().is_empty());
    assert!(be.read_range(&a, 50, 10).await.unwrap().is_empty());
    assert!(be.read_range(&a, 0, 0).await.unwrap().is_empty());

    // 덮어쓰기(posix-rename 또는 삭제 후 이름 변경)
    be.write(&a, WriteSource::Bytes(Bytes::from_static(b"HELLO")))
        .await
        .unwrap();
    assert_eq!(&be.read_range(&a, 0, 100).await.unwrap()[..], b"HELLO");

    // 1 MiB보다 큰 파일 업로드와 청크 경계를 걸치는 읽기
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big.bin");
    let data = pattern(3 * 1024 * 1024 + 123);
    std::fs::write(&src, &data).unwrap();
    let big = base.join("big.bin").unwrap();
    let e = be
        .write(
            &big,
            WriteSource::File {
                path: src,
                size: data.len() as u64,
            },
        )
        .await
        .unwrap();
    assert_eq!(e.size, data.len() as u64);
    let off = 1024 * 1024 - 7;
    let mid = be.read_range(&big, off as u64, 1_500_000).await.unwrap();
    assert_eq!(&mid[..], &data[off..off + 1_500_000]);
    let all = be.read_range(&big, 0, u64::MAX / 2).await.unwrap();
    assert_eq!(all.len(), data.len());
    assert!(all[..] == data[..]);

    // 여러 작업을 동시에
    let mut tasks = Vec::new();
    for i in 0..32u64 {
        let be = be.clone();
        let big = big.clone();
        tasks.push(tokio::spawn(async move {
            let off = i * 97_001;
            (off, be.read_range(&big, off, 70_000).await.unwrap())
        }));
    }
    for t in tasks {
        let (off, got) = t.await.unwrap();
        assert_eq!(&got[..], &data[off as usize..off as usize + 70_000]);
    }

    // 목록에는 임시 파일이 남지 않습니다.
    let sub = base.join("sub").unwrap();
    be.mkdir(&sub).await.unwrap();
    let mut names: Vec<_> = be
        .list(&base)
        .await
        .unwrap()
        .into_iter()
        .map(|e| (e.name, e.kind))
        .collect();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        names,
        [
            ("a.txt".to_string(), EntryKind::File),
            ("big.bin".to_string(), EntryKind::File),
            ("sub".to_string(), EntryKind::Dir),
        ]
    );
    assert!(matches!(be.list(&a).await, Err(Error::NotADirectory)));
    assert!(matches!(
        be.read_range(&sub, 0, 10).await,
        Err(Error::IsADirectory)
    ));

    // 이름 변경
    let b = base.join("b.txt").unwrap();
    be.write(&b, WriteSource::Bytes(Bytes::from_static(b"B")))
        .await
        .unwrap();
    assert!(matches!(
        be.rename(&a, &b, false).await,
        Err(Error::AlreadyExists)
    ));
    assert_eq!(&be.read_range(&b, 0, 10).await.unwrap()[..], b"B");
    be.rename(&a, &b, true).await.unwrap();
    assert_eq!(&be.read_range(&b, 0, 10).await.unwrap()[..], b"HELLO");
    assert!(matches!(be.stat(&a).await, Err(Error::NotFound)));
    assert!(matches!(
        be.rename(&a, &base.join("x").unwrap(), false).await,
        Err(Error::NotFound)
    ));
    let moved = sub.join("c.txt").unwrap();
    be.rename(&b, &moved, false).await.unwrap();
    let sub2 = base.join("sub2").unwrap();
    be.rename(&sub, &sub2, false).await.unwrap();
    let moved = sub2.join("c.txt").unwrap();
    assert_eq!(be.stat(&moved).await.unwrap().size, 5);

    // 수정 시각
    let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    be.set_mtime(&moved, t).await.unwrap();
    assert_eq!(be.stat(&moved).await.unwrap().modified, Some(t));

    let q = be.quota().await.unwrap();
    eprintln!("quota: {q:?}");

    // 삭제
    assert!(matches!(
        be.delete(&sub2, EntryKind::Dir).await,
        Err(Error::DirectoryNotEmpty)
    ));
    assert!(matches!(
        be.delete(&sub2, EntryKind::File).await,
        Err(Error::IsADirectory)
    ));
    be.delete(&moved, EntryKind::File).await.unwrap();
    be.delete(&sub2, EntryKind::Dir).await.unwrap();
    be.delete(&big, EntryKind::File).await.unwrap();
    assert!(matches!(
        be.delete(&big, EntryKind::File).await,
        Err(Error::NotFound)
    ));
    assert!(matches!(be.stat(&big).await, Err(Error::NotFound)));
    assert!(matches!(
        be.read_range(&big, 0, 1).await,
        Err(Error::NotFound)
    ));
    assert!(matches!(be.list(&sub2).await, Err(Error::NotFound)));
    assert!(be.list(&base).await.unwrap().is_empty());
    be.delete(&base, EntryKind::Dir).await.unwrap();
    assert!(be.stat(&RemotePath::root()).await.unwrap().is_dir());
}

#[tokio::test]
async fn sftp_rejects_untrusted_host_key() {
    let Some(mut cfg) = base_config() else { return };
    let dir = tempfile::tempdir().unwrap();

    // 기록 없음
    let empty = dir.path().join("empty_known_hosts");
    std::fs::write(&empty, "").unwrap();
    cfg.known_hosts = Some(empty);
    match SftpBackend::connect(&cfg, &EnvSecrets).await {
        Err(Error::Auth(msg)) => {
            eprintln!("{msg}");
            assert!(
                msg.contains("호스트 키") && msg.contains("SHA256:"),
                "{msg}"
            );
        }
        Err(e) => panic!("Auth 오류여야 합니다: {e}"),
        Ok(_) => panic!("신뢰하지 않는 호스트 키를 받아들였습니다"),
    }

    // 다른 키 기록
    let host = if cfg.port == 22 {
        cfg.host.clone()
    } else {
        format!("[{}]:{}", cfg.host, cfg.port)
    };
    let wrong = dir.path().join("wrong_known_hosts");
    std::fs::write(
        &wrong,
        format!(
            "{host} ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILixT8uYxgANE7LoChCHYdKLS0BiU3QETdh9dT0jko0+\n"
        ),
    )
    .unwrap();
    cfg.known_hosts = Some(wrong);
    match SftpBackend::connect(&cfg, &EnvSecrets).await {
        Err(Error::Auth(msg)) => {
            eprintln!("{msg}");
            assert!(msg.contains("다릅니다") && msg.contains("SHA256:"), "{msg}");
        }
        Err(e) => panic!("Auth 오류여야 합니다: {e}"),
        Ok(_) => panic!("다른 호스트 키를 받아들였습니다"),
    }
}

#[tokio::test]
async fn sftp_rejects_wrong_password() {
    struct Wrong;
    impl SecretResolver for Wrong {
        fn resolve(&self, _: &SecretSource) -> expandrv_core::Result<SecretString> {
            Ok(SecretString::from("definitely-wrong"))
        }
    }
    let Some(cfg) = base_config() else { return };
    if cfg.password.is_none() {
        return;
    }
    assert!(matches!(
        SftpBackend::connect(&cfg, &Wrong).await,
        Err(Error::Auth(_))
    ));
}

#[tokio::test]
async fn sftp_key_login() {
    let (Some(mut cfg), Some(user), Some(key)) = (base_config(), env("KEY_USER"), env("KEY"))
    else {
        eprintln!("EXPANDRV_TEST_SFTP_KEY_USER/KEY가 없어 키 로그인 테스트를 건너뜁니다");
        return;
    };
    cfg.username = user;
    cfg.password = None;
    cfg.private_key = Some(PathBuf::from(key));
    if env("KEY_PASSPHRASE").is_some() {
        cfg.key_passphrase = Some(SecretSource::Env {
            var: "EXPANDRV_TEST_SFTP_KEY_PASSPHRASE".into(),
        });
    }
    let be = SftpBackend::connect(&cfg, &EnvSecrets).await.unwrap();
    let f = p(&format!("/{}", unique("key")));
    be.write(&f, WriteSource::Bytes(Bytes::from_static(b"k")))
        .await
        .unwrap();
    assert_eq!(&be.read_range(&f, 0, 1).await.unwrap()[..], b"k");
    be.delete(&f, EntryKind::File).await.unwrap();
}

#[tokio::test]
async fn sftp_reconnects_after_server_restart() {
    let (Some(cfg), Some(container)) = (base_config(), env("CONTAINER")) else {
        eprintln!("EXPANDRV_TEST_SFTP_CONTAINER가 없어 재연결 테스트를 건너뜁니다");
        return;
    };
    let be = Arc::new(SftpBackend::connect(&cfg, &EnvSecrets).await.unwrap());
    be.stat(&RemotePath::root()).await.unwrap();
    let st = std::process::Command::new("docker")
        .args(["restart", "-t", "1", &container])
        .stdin(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(st.success());
    // 서버가 다시 뜰 때까지 기다립니다. 끊긴 연결은 다음 호출에서 다시 엽니다.
    let mut last = None;
    for _ in 0..30 {
        match be.list(&RemotePath::root()).await {
            Ok(_) => return,
            Err(e) => last = Some(e),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("재연결 실패: {last:?}");
}
