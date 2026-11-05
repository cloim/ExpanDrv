//! VFS를 WinFsp 볼륨으로 마운트합니다.

mod fs;
pub mod runtime;
pub mod security;

use expandrv_core::{Error, Result};
use expandrv_vfs::Vfs;
use fs::ExpanFs;
use std::sync::Arc;
use std::time::SystemTime;
use winfsp::host::{FileSystemHost, FileSystemParams, VolumeParams};

pub use fs::{filetime, from_filetime};

#[derive(Debug, Clone)]
pub struct MountOptions {
    /// `X:` 형태의 드라이브 문자 또는 존재하지 않는 폴더 경로
    pub mount_point: String,
    pub label: String,
    /// 커널 캐시가 파일 정보를 유지하는 시간(ms)
    pub file_info_timeout_ms: u32,
    pub serial: u32,
}

/// 마운트된 드라이브. 드롭하면 디스패처를 멈추고 마운트를 해제합니다.
pub struct MountedDrive {
    host: FileSystemHost<ExpanFs>,
    mount_point: String,
}

impl MountedDrive {
    pub fn mount(vfs: Arc<Vfs>, opts: &MountOptions) -> Result<Self> {
        runtime::init()?;
        let security = security::owner_only_descriptor()?;
        let mut vp = VolumeParams::new();
        vp.filesystem_name("ExpanDrv")
            .sector_size(4096)
            .sectors_per_allocation_unit(1)
            .max_component_length(255)
            .volume_creation_time(fs::filetime(SystemTime::now()))
            .volume_serial_number(opts.serial)
            .file_info_timeout(opts.file_info_timeout_ms)
            .case_sensitive_search(false)
            .case_preserved_names(true)
            .unicode_on_disk(true)
            .persistent_acls(true)
            .read_only_volume(vfs.read_only())
            // Cleanup 전에 캐시를 비워 업로드 시점에 모든 쓰기가 도착하게 합니다.
            .flush_and_purge_on_cleanup(true);

        let ctx = ExpanFs::new(vfs, opts.label.clone(), security);
        // 1초마다 VFS가 감지한 원격 변경을 탐색기에 알립니다.
        let mut host = FileSystemHost::new_with_timer::<Vec<expandrv_vfs::Change>, 1000>(
            FileSystemParams::default_params(vp),
            ctx,
        )
        .map_err(|e| fsp_err("파일 시스템 생성", e))?;
        host.mount(&opts.mount_point)
            .map_err(|e| fsp_err(&format!("{} 마운트", opts.mount_point), e))?;
        if let Err(e) = FileSystemHost::<ExpanFs>::start(&mut host) {
            host.unmount();
            return Err(fsp_err("디스패처 시작", e));
        }
        tracing::info!(mount_point = %opts.mount_point, "마운트 완료");
        Ok(Self {
            host,
            mount_point: opts.mount_point.clone(),
        })
    }

    pub fn mount_point(&self) -> &str {
        &self.mount_point
    }
}

fn fsp_err(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Config(format!("{what} 실패: {e}"))
}

impl Drop for MountedDrive {
    fn drop(&mut self) {
        self.host.stop();
        self.host.unmount();
        tracing::info!(mount_point = %self.mount_point, "마운트 해제");
    }
}
