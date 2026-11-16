//! Windows 로그인 시 시작 프로그램(시작 앱) 바로가기 등록.
//! `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup` 폴더에 바로가기를 등록하며 관리자 권한이 필요 없습니다.

use expandrv_core::{Error, Result};
use std::path::{Path, PathBuf};
use windows::core::{HSTRING, Interface};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize, IPersistFile, STGM_READ,
};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, RegDeleteKeyValueW,
};
use windows::Win32::UI::Shell::{IShellLinkW, ShellLink, SLR_NO_UI, SLR_NOSEARCH, SLR_NOTRACK};

pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const VALUE_NAME: &str = "ExpanDrv";
pub const SHORTCUT_NAME: &str = "ExpanDrv.lnk";

fn err<T: ToString>(msg: T) -> Error {
    Error::Io(std::io::Error::new(std::io::ErrorKind::Other, msg.to_string()))
}

struct ComGuard {
    need_uninit: bool,
}

impl ComGuard {
    fn new() -> Self {
        unsafe {
            let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let need_uninit = hr.is_ok();
            Self { need_uninit }
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.need_uninit {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

/// Windows 시작 프로그램(Startup) 폴더 경로를 반환합니다.
pub fn startup_folder() -> Result<PathBuf> {
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return Ok(PathBuf::from(appdata).join(r"Microsoft\Windows\Start Menu\Programs\Startup"));
    }
    Err(err("APPDATA 환경 변수를 찾을 수 없습니다."))
}

/// 기본 시작 앱 바로가기 파일 경로를 반환합니다.
pub fn default_shortcut_path() -> Result<PathBuf> {
    Ok(startup_folder()?.join(SHORTCUT_NAME))
}

/// 자동 실행 명령 문자열: 백그라운드 에이전트로 시작해 `auto_mount` 드라이브를 마운트합니다.
pub fn command_line(exe: &Path, cfg: &Path) -> String {
    format!(
        "\"{}\" --config \"{}\" agent --background --autostart",
        exe.display(),
        cfg.display()
    )
}

/// 바로가기 실행 인자 문자열을 반환합니다.
pub fn arguments(cfg: &Path) -> String {
    format!("--config \"{}\" agent --background --autostart", cfg.display())
}

/// 지정한 경로에 시작 앱 바로가기를 생성합니다.
pub fn enable_at(shortcut_path: &Path, exe: &Path, arguments: &str) -> Result<()> {
    let _com = ComGuard::new();

    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| err(format!("COM ShellLink 생성 실패: {e}")))?;

    unsafe {
        link.SetPath(&HSTRING::from(exe.as_os_str()))
            .map_err(|e| err(format!("바로가기 대상 경로 설정 실패: {e}")))?;

        if !arguments.is_empty() {
            link.SetArguments(&HSTRING::from(arguments))
                .map_err(|e| err(format!("바로가기 인수 설정 실패: {e}")))?;
        }

        if let Some(parent) = exe.parent() {
            link.SetWorkingDirectory(&HSTRING::from(parent.as_os_str()))
                .map_err(|e| err(format!("바로가기 작업 디렉터리 설정 실패: {e}")))?;
        }

        link.SetDescription(&HSTRING::from("ExpanDrv 자동 실행"))
            .map_err(|e| err(format!("바로가기 설명 설정 실패: {e}")))?;
    }

    let persist: IPersistFile = link
        .cast()
        .map_err(|e| err(format!("IPersistFile 인터페이스 획득 실패: {e}")))?;

    if let Some(parent) = shortcut_path.parent() {
        std::fs::create_dir_all(parent).map_err(Error::Io)?;
    }

    unsafe {
        persist
            .Save(&HSTRING::from(shortcut_path.as_os_str()), true)
            .map_err(|e| err(format!("바로가기 저장 실패: {e}")))?;
    }

    // 기존 레지스트리 Run 키 잔여물이 있다면 정리합니다.
    clean_legacy_run_key();

    Ok(())
}

/// 기본 시작 폴더에 ExpanDrv 바로가기를 생성하여 윈도우 시작 앱으로 등록합니다.
pub fn enable(exe: &Path, cfg: &Path) -> Result<()> {
    let shortcut = default_shortcut_path()?;
    let args = arguments(cfg);
    enable_at(&shortcut, exe, &args)
}

/// 지정한 바로가기를 삭제합니다.
pub fn disable_at(shortcut_path: &Path) -> Result<()> {
    if shortcut_path.exists() {
        std::fs::remove_file(shortcut_path).map_err(Error::Io)?;
    }
    clean_legacy_run_key();
    Ok(())
}

/// 기본 시작 폴더의 ExpanDrv 바로가기를 삭제하여 시작 앱 등록을 해제합니다.
pub fn disable() -> Result<()> {
    let shortcut = default_shortcut_path()?;
    disable_at(&shortcut)
}

/// 지정한 바로가기 파일의 대상 명령을 조회합니다. 등록이 없으면 `None`을 반환합니다.
pub fn status_at(shortcut_path: &Path) -> Result<Option<String>> {
    if !shortcut_path.exists() {
        return Ok(None);
    }

    let _com = ComGuard::new();

    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }
        .map_err(|e| err(format!("COM ShellLink 생성 실패: {e}")))?;

    let persist: IPersistFile = link
        .cast()
        .map_err(|e| err(format!("IPersistFile 인터페이스 획득 실패: {e}")))?;

    unsafe {
        persist
            .Load(&HSTRING::from(shortcut_path.as_os_str()), STGM_READ)
            .map_err(|e| err(format!("바로가기 파일 열기 실패: {e}")))?;
        let _ = link.Resolve(
            HWND(std::ptr::null_mut()),
            (SLR_NO_UI | SLR_NOSEARCH | SLR_NOTRACK).0 as u32,
        );
    }

    let mut target_buf = [0u16; 1024];
    unsafe {
        link.GetPath(&mut target_buf, std::ptr::null_mut(), 0)
            .map_err(|e| err(format!("바로가기 대상 읽기 실패: {e}")))?;
    }
    let target_len = target_buf.iter().position(|&c| c == 0).unwrap_or(target_buf.len());
    let target = String::from_utf16_lossy(&target_buf[..target_len]);

    let mut args_buf = [0u16; 2048];
    unsafe {
        link.GetArguments(&mut args_buf)
            .map_err(|e| err(format!("바로가기 인수 읽기 실패: {e}")))?;
    }
    let args_len = args_buf.iter().position(|&c| c == 0).unwrap_or(args_buf.len());
    let args = String::from_utf16_lossy(&args_buf[..args_len]);

    if args.is_empty() {
        Ok(Some(format!("\"{target}\"")))
    } else {
        Ok(Some(format!("\"{target}\" {args}")))
    }
}

/// 기본 시작 폴더의 ExpanDrv 시작 앱 등록 상태를 조회합니다.
pub fn status() -> Result<Option<String>> {
    let shortcut = default_shortcut_path()?;
    status_at(&shortcut)
}

/// 이전 버전에서 등록했던 레지스트리 Run 키 값을 정리합니다.
pub fn clean_legacy_run_key() {
    unsafe {
        let _ = RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(RUN_KEY),
            &HSTRING::from(VALUE_NAME),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_status_disable_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let shortcut = temp.path().join("ExpanDrvTest.lnk");
        let exe = Path::new(r"C:\Apps\expandrv.exe");
        let cfg = Path::new(r"C:\cfg\config.toml");
        let args = arguments(cfg);
        let expected_cmd = command_line(exe, cfg);

        assert_eq!(
            expected_cmd,
            r#""C:\Apps\expandrv.exe" --config "C:\cfg\config.toml" agent --background --autostart"#
        );

        assert_eq!(status_at(&shortcut).unwrap(), None);

        enable_at(&shortcut, exe, &args).unwrap();
        assert!(shortcut.exists());

        let registered = status_at(&shortcut).unwrap();
        assert_eq!(registered.as_deref(), Some(expected_cmd.as_str()));

        disable_at(&shortcut).unwrap();
        assert!(!shortcut.exists());
        assert_eq!(status_at(&shortcut).unwrap(), None);

        // 이미 없는 상태에서 disable 다시 호출해도 에러 없음
        disable_at(&shortcut).unwrap();
    }
}
