//! Windows 로그인 시 에이전트 자동 실행. 현재 사용자 `Run` 키만 쓰므로 관리자 권한이 필요 없습니다.

use expandrv_core::{Error, Result};
use std::path::Path;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows::core::HSTRING;

pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const VALUE_NAME: &str = "ExpanDrv";

fn win(e: WIN32_ERROR) -> Error {
    Error::Io(std::io::Error::from_raw_os_error(e.0 as i32))
}

/// 자동 실행 명령: 백그라운드 에이전트로 시작해 `auto_mount` 드라이브를 마운트합니다.
pub fn command_line(exe: &Path, cfg: &Path) -> String {
    format!(
        "\"{}\" --config \"{}\" agent --background --autostart",
        exe.display(),
        cfg.display()
    )
}

pub fn enable(key: &str, command: &str) -> Result<()> {
    let data: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
    let r = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(key),
            &HSTRING::from(VALUE_NAME),
            REG_SZ.0,
            Some(data.as_ptr().cast()),
            (data.len() * 2) as u32,
        )
    };
    if r.is_err() {
        return Err(win(r));
    }
    Ok(())
}

/// 등록이 없어도 성공으로 봅니다.
pub fn disable(key: &str) -> Result<()> {
    let r = unsafe {
        RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(key),
            &HSTRING::from(VALUE_NAME),
        )
    };
    if r.is_err() && r != ERROR_FILE_NOT_FOUND {
        return Err(win(r));
    }
    Ok(())
}

/// 등록된 명령. 없으면 `None`입니다.
pub fn status(key: &str) -> Result<Option<String>> {
    let mut buf = vec![0u16; 2048];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(key),
            &HSTRING::from(VALUE_NAME),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if r == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if r.is_err() {
        return Err(win(r));
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(Some(String::from_utf16_lossy(&buf[..len])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Registry::RegDeleteKeyW;

    #[test]
    fn enable_status_disable_roundtrip() {
        // 실제 Run 키 대신 테스트 전용 키를 씁니다.
        let key = format!(r"Software\ExpanDrvTest\Run-{}", std::process::id());
        let cmd = command_line(
            Path::new(r"C:\Apps\expandrv.exe"),
            Path::new(r"C:\cfg\config.toml"),
        );
        assert_eq!(
            cmd,
            r#""C:\Apps\expandrv.exe" --config "C:\cfg\config.toml" agent --background --autostart"#
        );
        assert_eq!(status(&key).unwrap(), None);
        enable(&key, &cmd).unwrap();
        assert_eq!(status(&key).unwrap().as_deref(), Some(cmd.as_str()));
        disable(&key).unwrap();
        assert_eq!(status(&key).unwrap(), None);
        disable(&key).unwrap();
        unsafe {
            let _ = RegDeleteKeyW(HKEY_CURRENT_USER, &HSTRING::from(key));
            let _ = RegDeleteKeyW(HKEY_CURRENT_USER, &HSTRING::from(r"Software\ExpanDrvTest"));
        }
    }
}
