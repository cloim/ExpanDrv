//! 볼륨 항목에 붙일 보안 서술자. 현재 사용자, SYSTEM, Administrators만 접근할 수 있습니다.

use expandrv_core::{Error, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{HSTRING, PWSTR};

fn win(e: windows::core::Error) -> Error {
    Error::Io(std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))
}

/// 현재 프로세스 사용자 SID 문자열(`S-1-5-21-...`)
pub fn current_user_sid() -> Result<String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(win)?;
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
        let sid = s
            .to_string()
            .map_err(|e| Error::Io(std::io::Error::other(e)));
        let _ = LocalFree(Some(HLOCAL(s.0.cast())));
        sid
    }
}

/// 소유자 전용 자체 상대 보안 서술자 바이트
pub fn owner_only_descriptor() -> Result<Vec<u8>> {
    let sid = current_user_sid()?;
    let sddl = format!("O:{sid}G:{sid}D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{sid})");
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let mut size = 0u32;
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(sddl),
            SDDL_REVISION_1,
            &mut sd,
            Some(&mut size),
        )
        .map_err(win)?;
        let bytes = std::slice::from_raw_parts(sd.0 as *const u8, size as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_names_current_user() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-5-"), "{sid}");
        let sd = owner_only_descriptor().unwrap();
        assert!(sd.len() > 20);
        assert_eq!(sd[0], 1, "SECURITY_DESCRIPTOR revision");
    }
}
