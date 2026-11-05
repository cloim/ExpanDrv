//! 프로세스 토큰의 사용자 SID 조회

use std::io;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::core::PWSTR;

fn win(e: windows::core::Error) -> io::Error {
    io::Error::from_raw_os_error(e.code().0 & 0xFFFF)
}

/// 프로세스 핸들 토큰의 사용자 SID 문자열
fn token_user_sid(process: HANDLE) -> io::Result<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(process, TOKEN_QUERY, &mut token).map_err(win)?;
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let info = GetTokenInformation(
            token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        );
        let _ = CloseHandle(token);
        info.map_err(win)?;
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut s).map_err(win)?;
        let sid = s.to_string().map_err(io::Error::other);
        let _ = LocalFree(Some(HLOCAL(s.0.cast())));
        sid
    }
}

/// 현재 프로세스 사용자 SID 문자열(`S-1-5-21-...`)
pub fn current_user_sid() -> io::Result<String> {
    token_user_sid(unsafe { GetCurrentProcess() })
}

/// 다른 프로세스의 사용자 SID 문자열
pub fn process_user_sid(pid: u32) -> io::Result<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).map_err(win)?;
        let sid = token_user_sid(h);
        let _ = CloseHandle(h);
        sid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_sid_matches() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-5-"), "{sid}");
        assert_eq!(process_user_sid(std::process::id()).unwrap(), sid);
    }
}
