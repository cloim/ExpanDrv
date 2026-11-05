//! 스토리지 백엔드 모음. 네트워크 백엔드는 기능 플래그로 분리합니다.

use expandrv_core::config::{BackendConfig, SecretResolver};
use expandrv_core::{Backend, Error, Result};
use std::sync::Arc;

#[cfg(feature = "ftp")]
pub mod ftp;
pub mod local;
#[cfg(feature = "s3")]
pub mod s3;
#[cfg(feature = "sftp")]
pub mod sftp;
#[cfg(feature = "webdav")]
pub mod webdav;

/// 설정으로 백엔드를 만들고 연결과 인증을 확인합니다.
pub async fn connect(
    cfg: &BackendConfig,
    secrets: &dyn SecretResolver,
) -> Result<Arc<dyn Backend>> {
    match cfg {
        BackendConfig::Local { root } => Ok(Arc::new(local::LocalBackend::new(root)?)),
        #[cfg(feature = "webdav")]
        BackendConfig::Webdav(c) => Ok(Arc::new(webdav::WebDavBackend::connect(c, secrets).await?)),
        #[cfg(feature = "sftp")]
        BackendConfig::Sftp(c) => Ok(Arc::new(sftp::SftpBackend::connect(c, secrets).await?)),
        #[cfg(feature = "s3")]
        BackendConfig::S3(c) => Ok(Arc::new(s3::S3Backend::connect(c, secrets).await?)),
        #[cfg(feature = "ftp")]
        BackendConfig::Ftp(c) => Ok(Arc::new(ftp::FtpBackend::connect(c, secrets).await?)),
        #[allow(unreachable_patterns)]
        other => {
            let _ = secrets;
            Err(Error::Unsupported(other.kind()))
        }
    }
}
