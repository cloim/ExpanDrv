//! 설치된 WinFsp 런타임 DLL을 레지스트리 경로에서 로드합니다.
//!
//! `winfsp` 크레이트의 `system` 기능은 빌드 시 SDK 헤더를 요구하므로 쓰지 않고,
//! 여기서 전체 경로로 먼저 로드해 지연 로드가 같은 모듈을 찾게 합니다.

use expandrv_core::{Error, Result};
use std::path::PathBuf;
use std::sync::OnceLock;
use windows::Win32::System::LibraryLoader::LoadLibraryW;
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::core::{HSTRING, PCWSTR, w};

const DLL_NAME: &str = "winfsp-x64.dll";

fn install_dir(subkey: PCWSTR) -> Option<PathBuf> {
    let mut buf = [0u16; 512];
    let mut size = (buf.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey,
            w!("InstallDir"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// 설치된 런타임 DLL 경로
pub fn runtime_dll() -> Option<PathBuf> {
    install_dir(w!("SOFTWARE\\WOW6432Node\\WinFsp"))
        .or_else(|| install_dir(w!("SOFTWARE\\WinFsp")))
        .map(|d| d.join("bin").join(DLL_NAME))
        .filter(|p| p.is_file())
}

/// 프로세스당 한 번 런타임을 로드하고 초기화합니다.
pub fn init() -> Result<()> {
    static INIT: OnceLock<Result<(), String>> = OnceLock::new();
    INIT.get_or_init(|| {
        let dll = runtime_dll().ok_or("WinFsp 런타임이 설치되어 있지 않습니다")?;
        unsafe { LoadLibraryW(&HSTRING::from(dll.as_os_str())) }
            .map_err(|e| format!("{} 로드 실패: {e}", dll.display()))?;
        winfsp::winfsp_init().map_err(|e| format!("WinFsp 초기화 실패: {e:?}"))?;
        Ok(())
    })
    .clone()
    .map_err(Error::Config)
}
