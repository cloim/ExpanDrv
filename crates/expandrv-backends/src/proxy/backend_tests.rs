//! 실제 서버를 테스트 프록시 뒤에 두고 백엔드 트래픽이 프록시를 거치는지 확인합니다.
//! 각 테스트는 해당 백엔드 통합 테스트와 같은 `EXPANDRV_TEST_*` 환경 변수가 있을 때만 실행합니다.

use super::Proxy;
use super::test_proxy::{FakeProxy, ProxyMode};
use bytes::Bytes;
use expandrv_core::config::{EnvSecrets, ProxyConfig, SecretSource};
use expandrv_core::{Backend, EntryKind, RemotePath, WriteSource};
use std::sync::Arc;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

async fn proxy(mode: ProxyMode) -> (FakeProxy, Arc<Proxy>) {
    // SAFETY: 이 모듈 테스트만 쓰는 고유 변수이며 모두 같은 값을 씁니다.
    unsafe { std::env::set_var("EXPANDRV_TEST_PROXY_IT_PASS", "proxy-pw") };
    let fake = FakeProxy::start(mode, Some(("puser", "proxy-pw"))).await;
    let scheme = match mode {
        ProxyMode::Socks5 => "socks5h",
        ProxyMode::Http => "http",
    };
    let cfg = ProxyConfig {
        url: format!("{scheme}://127.0.0.1:{}", fake.port),
        username: Some("puser".into()),
        password: Some(SecretSource::Env {
            var: "EXPANDRV_TEST_PROXY_IT_PASS".into(),
        }),
    };
    let p = Arc::new(Proxy::from_config(&cfg, &EnvSecrets).unwrap());
    (fake, p)
}

/// 디렉터리 생성, 쓰기, 부분 읽기, 목록, 삭제를 한 번씩 수행합니다.
async fn exercise(be: &dyn Backend, tag: &str) {
    let dir = RemotePath::root()
        .join(&format!("proxy-{tag}-{}", std::process::id()))
        .unwrap();
    let _ = be
        .delete(&dir.join("f.bin").unwrap(), EntryKind::File)
        .await;
    let _ = be.delete(&dir, EntryKind::Dir).await;
    be.mkdir(&dir).await.unwrap();
    let file = dir.join("f.bin").unwrap();
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    be.write(&file, WriteSource::Bytes(Bytes::from(data.clone())))
        .await
        .unwrap();
    let part = be.read_range(&file, 100_000, 1000).await.unwrap();
    assert_eq!(&part[..], &data[100_000..101_000]);
    let names: Vec<String> = be
        .list(&dir)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["f.bin"]);
    be.delete(&file, EntryKind::File).await.unwrap();
    be.delete(&dir, EntryKind::Dir).await.unwrap();
}

#[tokio::test]
#[cfg(feature = "webdav")]
async fn webdav_through_socks5h() {
    let Some(url) = env("EXPANDRV_TEST_WEBDAV_URL") else {
        eprintln!("EXPANDRV_TEST_WEBDAV_URL이 없어 건너뜁니다");
        return;
    };
    let cfg = expandrv_core::config::WebDavConfig {
        url: url.clone(),
        username: env("EXPANDRV_TEST_WEBDAV_USER"),
        password: env("EXPANDRV_TEST_WEBDAV_PASS").map(|_| SecretSource::Env {
            var: "EXPANDRV_TEST_WEBDAV_PASS".into(),
        }),
        pinned_cert_sha256: None,
    };
    let (fake, p) = proxy(ProxyMode::Socks5).await;
    let be = crate::webdav::WebDavBackend::connect_via(&cfg, &EnvSecrets, Some(p))
        .await
        .unwrap();
    exercise(&be, "webdav").await;
    let authority = url.split("://").nth(1).unwrap().trim_end_matches('/');
    assert!(
        fake.targets().iter().all(|t| t == authority),
        "{:?}",
        fake.targets()
    );
    assert!(!fake.targets().is_empty());
}

#[cfg(feature = "sftp")]
async fn sftp_via(mode: ProxyMode) {
    let (Some(host), Some(user), Some(kh)) = (
        env("EXPANDRV_TEST_SFTP_HOST"),
        env("EXPANDRV_TEST_SFTP_USER"),
        env("EXPANDRV_TEST_SFTP_KNOWN_HOSTS"),
    ) else {
        eprintln!("EXPANDRV_TEST_SFTP_HOST/USER/KNOWN_HOSTS가 없어 건너뜁니다");
        return;
    };
    let port: u16 = env("EXPANDRV_TEST_SFTP_PORT").map_or(22, |p| p.parse().unwrap());
    let cfg = expandrv_core::config::SftpConfig {
        ssh_shell: false,
        use_ssh_config: false,
        host: host.clone(),
        port,
        username: user,
        password: Some(SecretSource::Env {
            var: "EXPANDRV_TEST_SFTP_PASSWORD".into(),
        }),
        private_key: None,
        key_passphrase: None,
        root: env("EXPANDRV_TEST_SFTP_ROOT").unwrap_or_else(|| "/data".into()),
        known_hosts: Some(kh.into()),
    };
    let (fake, p) = proxy(mode).await;
    let be = crate::sftp::SftpBackend::connect_via(&cfg, &EnvSecrets, Some(p))
        .await
        .unwrap();
    exercise(&be, &format!("sftp-{mode:?}")).await;
    assert_eq!(fake.targets(), [format!("{host}:{port}")]);
}

#[tokio::test]
#[cfg(feature = "sftp")]
async fn sftp_through_socks5h() {
    sftp_via(ProxyMode::Socks5).await;
}

#[tokio::test]
#[cfg(feature = "sftp")]
async fn sftp_through_http_connect() {
    sftp_via(ProxyMode::Http).await;
}

#[cfg(feature = "ftp")]
async fn ftp_via(mode: ProxyMode) {
    let (Some(host), Some(port)) = (env("EXPANDRV_TEST_FTP_HOST"), env("EXPANDRV_TEST_FTP_PORT"))
    else {
        eprintln!("EXPANDRV_TEST_FTP_HOST/PORT가 없어 건너뜁니다");
        return;
    };
    let port: u16 = port.parse().unwrap();
    let cfg = expandrv_core::config::FtpConfig {
        pinned_cert_sha256: None,
        host: host.clone(),
        port,
        username: env("EXPANDRV_TEST_FTP_USER").unwrap_or_else(|| "u".into()),
        password: Some(SecretSource::Env {
            var: "EXPANDRV_TEST_FTP_PASS".into(),
        }),
        tls: expandrv_core::config::FtpTls::None,
        encoding: "utf-8".into(),
        root: env("EXPANDRV_TEST_FTP_ROOT").unwrap_or_else(|| ".".into()),
    };
    let (fake, p) = proxy(mode).await;
    let be = crate::ftp::FtpBackend::connect_via(&cfg, &EnvSecrets, Some(p))
        .await
        .unwrap();
    exercise(&be, &format!("ftp-{mode:?}")).await;
    // 제어 연결과 데이터 연결 모두 FTP 호스트로 프록시를 거칩니다.
    let targets = fake.targets();
    assert!(targets.contains(&format!("{host}:{port}")), "{targets:?}");
    let data = targets
        .iter()
        .filter(|t| **t != format!("{host}:{port}"))
        .count();
    assert!(
        data >= 2,
        "데이터 연결이 프록시를 거치지 않았습니다: {targets:?}"
    );
    assert!(
        targets.iter().all(|t| t.starts_with(&format!("{host}:"))),
        "{targets:?}"
    );
}

#[tokio::test]
#[cfg(feature = "ftp")]
async fn ftp_through_socks5h() {
    ftp_via(ProxyMode::Socks5).await;
}

#[tokio::test]
#[cfg(feature = "ftp")]
async fn ftp_through_http_connect() {
    ftp_via(ProxyMode::Http).await;
}
