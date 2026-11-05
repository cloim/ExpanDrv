//! 스토리지 백엔드 모음. 네트워크 백엔드는 기능 플래그로 분리합니다.

use expandrv_core::config::{BackendConfig, ProxyConfig, SecretResolver};
use expandrv_core::{Backend, Error, Result};
use std::sync::Arc;

#[cfg(feature = "ftp")]
pub mod ftp;
pub mod local;
pub mod proxy;
#[cfg(feature = "s3")]
pub mod s3;
#[cfg(feature = "sftp")]
pub mod sftp;
#[cfg(feature = "webdav")]
pub mod webdav;

/// reqwest가 쓸 프로세스 기본 rustls 프로바이더로 ring을 설치합니다. HTTP 클라이언트를 만들기 전에 호출합니다.
#[cfg(any(feature = "webdav", feature = "s3"))]
pub(crate) fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // 다른 곳에서 이미 설치했다면 그 설정을 그대로 씁니다.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// 설정으로 백엔드를 만들고 연결과 인증을 확인합니다.
pub async fn connect(
    cfg: &BackendConfig,
    proxy: Option<&ProxyConfig>,
    secrets: &dyn SecretResolver,
) -> Result<Arc<dyn Backend>> {
    let proxy = proxy
        .map(|p| proxy::Proxy::from_config(p, secrets))
        .transpose()?
        .map(Arc::new);
    if proxy.is_some() && matches!(cfg, BackendConfig::Local { .. }) {
        return Err(Error::Config(
            "로컬 폴더 드라이브에는 프록시를 쓸 수 없습니다".into(),
        ));
    }
    match cfg {
        BackendConfig::Local { root } => Ok(Arc::new(local::LocalBackend::new(root)?)),
        #[cfg(feature = "webdav")]
        BackendConfig::Webdav(c) => Ok(Arc::new(
            webdav::WebDavBackend::connect_via(c, secrets, proxy).await?,
        )),
        #[cfg(feature = "sftp")]
        BackendConfig::Sftp(c) => Ok(Arc::new(
            sftp::SftpBackend::connect_via(c, secrets, proxy).await?,
        )),
        #[cfg(feature = "s3")]
        BackendConfig::S3(c) => Ok(Arc::new(
            s3::S3Backend::connect_via(c, secrets, proxy).await?,
        )),
        #[cfg(feature = "ftp")]
        BackendConfig::Ftp(c) => Ok(Arc::new(
            ftp::FtpBackend::connect_via(c, secrets, proxy).await?,
        )),
        #[allow(unreachable_patterns)]
        other => {
            let _ = secrets;
            Err(Error::Unsupported(other.kind()))
        }
    }
}
