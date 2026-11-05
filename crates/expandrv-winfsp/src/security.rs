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

/// 폴더에 현재 사용자와 SYSTEM만 접근하는 보호 DACL을 적용합니다. 하위 항목에 상속됩니다.
/// 캐시, 스테이징, 메타 DB처럼 원격 파일 내용과 이름이 남는 폴더에 씁니다.
pub fn protect_dir(path: &std::path::Path) -> Result<()> {
    use windows::Win32::Security::Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW};
    use windows::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
        PROTECTED_DACL_SECURITY_INFORMATION,
    };
    use windows::core::BOOL;

    let sid = current_user_sid()?;
    let sddl = format!("D:P(A;OICI;FA;;;{sid})(A;OICI;FA;;;SY)");
    unsafe {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &HSTRING::from(sddl),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
        .map_err(win)?;
        let mut present = BOOL::default();
        let mut defaulted = BOOL::default();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let got = GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted);
        let r = got.map_err(win).and_then(|()| {
            let status = SetNamedSecurityInfoW(
                &HSTRING::from(path.as_os_str()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(dacl),
                None,
            );
            if status.is_err() {
                Err(Error::Io(std::io::Error::from_raw_os_error(
                    status.0 as i32,
                )))
            } else {
                Ok(())
            }
        });
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 폴더의 DACL을 SDDL 문자열로 읽습니다.
    fn dacl_sddl(path: &std::path::Path) -> String {
        use windows::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW,
            SE_FILE_OBJECT,
        };
        use windows::Win32::Security::DACL_SECURITY_INFORMATION;
        unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            let st = GetNamedSecurityInfoW(
                &HSTRING::from(path.as_os_str()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                None,
                None,
                &mut sd,
            );
            assert!(st.is_ok(), "{st:?}");
            let mut s = PWSTR::null();
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut s,
                None,
            )
            .unwrap();
            let out = s.to_string().unwrap();
            let _ = LocalFree(Some(HLOCAL(s.0.cast())));
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            out
        }
    }

    #[test]
    fn protect_dir_allows_only_user_and_system() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("sub");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("f.bin"), b"x").unwrap();
        protect_dir(dir.path()).unwrap();
        let sid = current_user_sid().unwrap();
        let top = dacl_sddl(dir.path());
        assert!(top.starts_with("D:P"), "{top}");
        assert!(top.contains(&sid) && top.contains(";;;SY)"), "{top}");
        assert_eq!(top.matches("(A;").count(), 2, "{top}");
        // 기존 하위 항목에도 상속되어 다른 계정 항목이 남지 않습니다.
        let inner = dacl_sddl(&child.join("f.bin"));
        assert_eq!(inner.matches("(A;").count(), 2, "{inner}");
        assert!(
            !inner.contains(";;;BU)") && !inner.contains(";;;AU)"),
            "{inner}"
        );
    }

    #[test]
    fn descriptor_names_current_user() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-5-"), "{sid}");
        let sd = owner_only_descriptor().unwrap();
        assert!(sd.len() > 20);
        assert_eq!(sd[0], 1, "SECURITY_DESCRIPTOR revision");
    }
}
