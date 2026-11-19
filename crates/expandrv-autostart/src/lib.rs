//! Windows 로그인 자동 실행(시작 앱) 등록.
//!
//! 현재 사용자 `Run` 키에 GUI 실행 파일(`expandrv-gui.exe --autostart`)을 등록합니다.
//! GUI는 창 없이(Windows 서브시스템) 에이전트만 띄우고 바로 끝나므로 로그인할 때 터미널 창이 뜨지 않으며,
//! Windows 설정의 `시작 앱` 목록에 ExpanDrv로 표시됩니다. 관리자 권한이 필요 없습니다.

use expandrv_core::{Error, Result};
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_DWORD, REG_ROUTINE_FLAGS, REG_SZ, REG_VALUE_TYPE, RRF_RT_REG_BINARY,
    RRF_RT_REG_DWORD, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
    RegSetKeyValueW,
};
use windows::core::HSTRING;

pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// Windows `시작 앱`에서 사용자가 끈 항목을 기록하는 키
pub const APPROVED_RUN_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";
pub const APPROVED_FOLDER_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\StartupFolder";
/// 사용자가 자동 실행을 직접 끈 사실을 남기는 키(설치 프로그램이 다시 켜지 않도록)
pub const PREF_KEY: &str = r"Software\ExpanDrv";
pub const OPT_OUT_VALUE: &str = "AutostartOptOut";
pub const VALUE_NAME: &str = "ExpanDrv";
/// GUI 실행 파일 이름. CLI와 같은 폴더에 설치됩니다.
pub const GUI_EXE: &str = "expandrv-gui.exe";
/// 0.1.12에서 쓰던 시작 프로그램 폴더 바로가기 이름(정리 대상)
pub const LEGACY_SHORTCUT: &str = "ExpanDrv.lnk";

fn win(e: WIN32_ERROR) -> Error {
    Error::Io(std::io::Error::from_raw_os_error(e.0 as i32))
}

/// 자동 실행에 쓰는 레지스트리 위치. 테스트는 임시 키를 넘깁니다.
#[derive(Clone, Debug)]
pub struct Keys {
    pub run: String,
    pub approved: String,
    pub pref: String,
}

impl Keys {
    pub fn user() -> Self {
        Self {
            run: RUN_KEY.into(),
            approved: APPROVED_RUN_KEY.into(),
            pref: PREF_KEY.into(),
        }
    }
}

/// CLI 실행 파일 옆의 GUI 실행 파일 경로
pub fn gui_exe_for(cli: &Path) -> PathBuf {
    cli.with_file_name(GUI_EXE)
}

/// 자동 실행 명령: GUI가 창 없이 에이전트를 띄워 `auto_mount` 드라이브를 마운트합니다.
pub fn command_line(gui: &Path, cfg: &Path) -> String {
    format!(
        "\"{}\" --config \"{}\" --autostart",
        gui.display(),
        cfg.display()
    )
}

fn set_value(key: &str, name: &str, kind: REG_VALUE_TYPE, data: &[u8]) -> Result<()> {
    let r = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(key),
            &HSTRING::from(name),
            kind.0,
            Some(data.as_ptr().cast()),
            data.len() as u32,
        )
    };
    if r.is_err() {
        return Err(win(r));
    }
    Ok(())
}

/// 값이 없어도 성공으로 봅니다.
fn delete_value(key: &str, name: &str) -> Result<()> {
    let r =
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, &HSTRING::from(key), &HSTRING::from(name)) };
    if r.is_err() && r != ERROR_FILE_NOT_FOUND && r.0 != 3 {
        // 3: ERROR_PATH_NOT_FOUND(키 자체가 없음)
        return Err(win(r));
    }
    Ok(())
}

/// 값의 원시 바이트. 없으면 `None`입니다.
fn get_value(key: &str, name: &str, flags: REG_ROUTINE_FLAGS) -> Result<Option<Vec<u8>>> {
    let mut buf = vec![0u8; 8192];
    let mut size = buf.len() as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(key),
            &HSTRING::from(name),
            flags,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if r == ERROR_FILE_NOT_FOUND || r.0 == 3 {
        return Ok(None);
    }
    if r.is_err() {
        return Err(win(r));
    }
    buf.truncate(size as usize);
    Ok(Some(buf))
}

fn utf16_string(bytes: &[u8]) -> String {
    let wide: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| u16::from_le_bytes(c))
        .take_while(|&c| c != 0)
        .collect();
    String::from_utf16_lossy(&wide)
}

/// Windows `시작 앱` 화면에서 꺼 둔 항목인지 확인합니다(첫 바이트가 홀수면 꺼짐).
fn approved_disabled(key: &str, name: &str) -> bool {
    matches!(
        get_value(key, name, RRF_RT_REG_BINARY),
        Ok(Some(b)) if b.first().is_some_and(|f| f & 1 == 1)
    )
}

fn opted_out(keys: &Keys) -> bool {
    matches!(
        get_value(&keys.pref, OPT_OUT_VALUE, RRF_RT_REG_DWORD),
        Ok(Some(b)) if b.iter().any(|&x| x != 0)
    )
}

/// 지정한 키에 자동 실행을 등록하고, 사용자가 직접 껐다는 기록과 `시작 앱`의 꺼짐 상태를 지웁니다.
pub fn enable_in(keys: &Keys, command: &str) -> Result<()> {
    let data: Vec<u8> = command
        .encode_utf16()
        .chain(Some(0))
        .flat_map(u16::to_le_bytes)
        .collect();
    set_value(&keys.run, VALUE_NAME, REG_SZ, &data)?;
    delete_value(&keys.approved, VALUE_NAME)?;
    delete_value(&keys.pref, OPT_OUT_VALUE)?;
    Ok(())
}

/// 지정한 키에서 자동 실행 등록을 지웁니다. `opt_out`이면 설치 프로그램이 다시 켜지 않도록 기록합니다.
/// 제거 프로그램(`opt_out == false`)은 업그레이드 중에도 실행되므로 사용자가 끈 기록은 건드리지 않습니다.
pub fn disable_in(keys: &Keys, opt_out: bool) -> Result<()> {
    delete_value(&keys.run, VALUE_NAME)?;
    if opt_out {
        delete_value(&keys.approved, VALUE_NAME)?;
        set_value(&keys.pref, OPT_OUT_VALUE, REG_DWORD, &1u32.to_le_bytes())?;
    }
    Ok(())
}

/// 등록된 명령. 없거나 Windows `시작 앱`에서 꺼 두었으면 `None`입니다.
pub fn status_in(keys: &Keys) -> Result<Option<String>> {
    let Some(raw) = get_value(&keys.run, VALUE_NAME, RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ)? else {
        return Ok(None);
    };
    if approved_disabled(&keys.approved, VALUE_NAME) {
        return Ok(None);
    }
    Ok(Some(utf16_string(&raw)))
}

/// 설치 프로그램용: 사용자가 끄지 않았으면 등록합니다. 등록했으면 `true`입니다.
pub fn install_in(keys: &Keys, command: &str, legacy_disabled: bool) -> Result<bool> {
    if opted_out(keys) || legacy_disabled || approved_disabled(&keys.approved, VALUE_NAME) {
        return Ok(false);
    }
    enable_in(keys, command)?;
    Ok(true)
}

/// GUI 실행 파일을 시작 앱으로 등록합니다.
pub fn enable(gui: &Path, cfg: &Path) -> Result<()> {
    enable_in(&Keys::user(), &command_line(gui, cfg))?;
    clean_legacy();
    Ok(())
}

/// 사용자가 끈 경우: 등록을 지우고, 다시 설치해도 켜지 않도록 기록합니다.
pub fn disable() -> Result<()> {
    disable_in(&Keys::user(), true)?;
    clean_legacy();
    Ok(())
}

/// 등록 명령이 지정한 GUI 실행 파일을 가리키는지 확인합니다(대소문자 무시).
pub fn points_to(command: &str, gui: &Path) -> bool {
    let want = format!("\"{}\"", gui.display()).to_lowercase();
    command.trim_start().to_lowercase().starts_with(&want)
}

/// 제거 프로그램용: 지정한 키의 등록이 `gui`를 가리킬 때만 지웁니다. 지웠으면 `true`입니다.
pub fn uninstall_in(keys: &Keys, gui: &Path) -> Result<bool> {
    let Some(raw) = get_value(&keys.run, VALUE_NAME, RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ)? else {
        return Ok(false);
    };
    if !points_to(&utf16_string(&raw), gui) {
        return Ok(false);
    }
    disable_in(keys, false)?;
    Ok(true)
}

/// 제거 프로그램용: 이 설치본의 GUI를 가리키는 등록만 지웁니다. 사용자가 끈 기록은 남겨
/// 다시 설치해도 그 선택을 따르고, 다른 위치에 설치한 ExpanDrv의 등록은 건드리지 않습니다.
pub fn uninstall(gui: &Path) -> Result<bool> {
    uninstall_in(&Keys::user(), gui)
}

/// 설치 프로그램용: 새로 설치하면 등록하고, 업데이트면 이전 등록(바로가기 포함)을 새 방식으로 옮깁니다.
/// 사용자가 끈 상태(ExpanDrv 설정 또는 Windows `시작 앱`)는 그대로 둡니다.
pub fn install(gui: &Path, cfg: &Path) -> Result<bool> {
    let legacy_disabled = approved_disabled(APPROVED_FOLDER_KEY, LEGACY_SHORTCUT);
    let r = install_in(&Keys::user(), &command_line(gui, cfg), legacy_disabled)?;
    clean_legacy();
    Ok(r)
}

/// 현재 사용자의 자동 실행 등록 명령
pub fn status() -> Result<Option<String>> {
    status_in(&Keys::user())
}

fn legacy_shortcut_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|appdata| {
        PathBuf::from(appdata)
            .join(r"Microsoft\Windows\Start Menu\Programs\Startup")
            .join(LEGACY_SHORTCUT)
    })
}

/// 0.1.12의 시작 프로그램 폴더 바로가기(콘솔 창이 뜨던 방식)를 지웁니다.
pub fn clean_legacy() {
    if let Some(path) = legacy_shortcut_path() {
        let _ = std::fs::remove_file(path);
    }
    let _ = delete_value(APPROVED_FOLDER_KEY, LEGACY_SHORTCUT);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempKeys(Keys, String);

    impl TempKeys {
        fn new() -> Self {
            let root = format!(r"Software\ExpanDrvTest-{}", std::process::id());
            let keys = Keys {
                run: format!(r"{root}\Run"),
                approved: format!(r"{root}\Approved"),
                pref: format!(r"{root}\Pref"),
            };
            Self(keys, root)
        }
    }

    impl Drop for TempKeys {
        fn drop(&mut self) {
            use windows::Win32::System::Registry::RegDeleteTreeW;
            unsafe {
                let _ = RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(self.1.as_str()));
            }
        }
    }

    #[test]
    fn command_points_to_gui_without_console() {
        let gui = gui_exe_for(Path::new(r"C:\Apps\ExpanDrv\expandrv.exe"));
        assert_eq!(gui, PathBuf::from(r"C:\Apps\ExpanDrv\expandrv-gui.exe"));
        assert_eq!(
            command_line(&gui, Path::new(r"C:\cfg\config.toml")),
            r#""C:\Apps\ExpanDrv\expandrv-gui.exe" --config "C:\cfg\config.toml" --autostart"#
        );
    }

    #[test]
    fn enable_status_disable_install_roundtrip() {
        let t = TempKeys::new();
        let keys = &t.0;
        let cmd = r#""C:\한글 경로\expandrv-gui.exe" --config "C:\cfg\config.toml" --autostart"#;

        assert_eq!(status_in(keys).unwrap(), None);
        // 새로 설치: 등록합니다.
        assert!(install_in(keys, cmd, false).unwrap());
        assert_eq!(status_in(keys).unwrap().as_deref(), Some(cmd));

        // Windows `시작 앱`에서 끄면 꺼짐으로 보고, 설치 프로그램도 다시 켜지 않습니다.
        set_value(
            &keys.approved,
            VALUE_NAME,
            windows::Win32::System::Registry::REG_BINARY,
            &[3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        )
        .unwrap();
        assert_eq!(status_in(keys).unwrap(), None);
        assert!(!install_in(keys, cmd, false).unwrap());
        // 직접 켜면 꺼짐 표시를 지웁니다.
        enable_in(keys, cmd).unwrap();
        assert_eq!(status_in(keys).unwrap().as_deref(), Some(cmd));

        // 제거(업그레이드 포함) 뒤 설치하면 다시 등록합니다. 이전 바로가기를 꺼 두었으면 따릅니다.
        disable_in(keys, false).unwrap();
        assert_eq!(status_in(keys).unwrap(), None);
        assert!(!install_in(keys, cmd, true).unwrap());
        assert!(install_in(keys, cmd, false).unwrap());
        assert_eq!(status_in(keys).unwrap().as_deref(), Some(cmd));

        // 사용자가 끄면 제거 후 재설치해도 켜지 않습니다.
        disable_in(keys, true).unwrap();
        assert_eq!(status_in(keys).unwrap(), None);
        disable_in(keys, false).unwrap();
        assert!(!install_in(keys, cmd, false).unwrap());
        assert_eq!(status_in(keys).unwrap(), None);
        // 직접 다시 켜면 끔 기록을 지웁니다.
        enable_in(keys, cmd).unwrap();
        disable_in(keys, false).unwrap();
        assert!(install_in(keys, cmd, false).unwrap());

        // 제거 프로그램은 자신의 GUI를 가리키는 등록만 지웁니다.
        let other = Path::new(r"D:\다른 설치\expandrv-gui.exe");
        assert!(!uninstall_in(keys, other).unwrap());
        assert!(status_in(keys).unwrap().is_some());
        let mine = Path::new(r"c:\한글 경로\EXPANDRV-GUI.exe");
        assert!(uninstall_in(keys, mine).unwrap());
        assert_eq!(status_in(keys).unwrap(), None);
        assert!(!uninstall_in(keys, mine).unwrap());

        // 없는 상태에서 다시 지워도 오류가 없습니다.
        disable_in(keys, false).unwrap();
        disable_in(keys, false).unwrap();
    }
}
