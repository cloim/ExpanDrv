//! 비밀 값을 Windows 자격 증명 관리자에 저장합니다. 값은 DPAPI(현재 사용자)로 한 번 더 암호화하고,
//! 설정 파일에는 [`SecretSource::Credential`] 참조만 둡니다.

mod dpapi;

use expandrv_core::config::{EnvSecrets, SecretResolver, SecretSource};
use expandrv_core::{Error, Result};
use secrecy::{ExposeSecret, SecretString};
use std::io;
use std::ptr;
use windows::Win32::Foundation::ERROR_NOT_FOUND;
use windows::Win32::Security::Credentials::{
    CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW,
    CredDeleteW, CredEnumerateW, CredFree, CredReadW, CredWriteW,
};
use windows::core::{PCWSTR, PWSTR};

/// 자격 증명 항목의 사용자 이름. 이 프로그램이 만든 항목을 구분하는 표시입니다.
const USER_NAME: &str = "ExpanDrv";

/// 대상 이름 최대 길이(UTF-16 단위). API 한도(32767)보다 작게 둡니다.
pub const MAX_TARGET_LEN: usize = 256;

/// 평문 비밀 값 최대 크기(바이트). DPAPI 암호문은 머리글과 블록 패딩, HMAC으로 약 230바이트 커지므로
/// (Windows 11 실측 한계 2335바이트) 자격 증명 BLOB 한도(2560바이트) 안에서 여유를 두고 제한합니다.
pub const MAX_SECRET_BYTES: usize = 2048;

/// `ExpanDrv/<drive_id>/<field>` 형태의 대상 이름을 만듭니다.
pub fn target_for(drive_id: &str, field: &str) -> String {
    format!("ExpanDrv/{drive_id}/{field}")
}

/// 현재 사용자의 자격 증명 관리자(일반 자격 증명, 이 컴퓨터에만 보관)
#[derive(Debug, Default, Clone, Copy)]
pub struct CredentialStore;

impl CredentialStore {
    /// 값을 DPAPI로 암호화해 저장합니다. 같은 대상이 있으면 덮어씁니다.
    pub fn set(&self, target: &str, secret: &SecretString) -> Result<()> {
        let mut name = wide_target(target)?;
        let plain = secret.expose_secret().as_bytes();
        if plain.len() > MAX_SECRET_BYTES {
            return Err(Error::Config(format!(
                "비밀 값은 최대 {MAX_SECRET_BYTES}바이트까지 저장할 수 있습니다"
            )));
        }
        let mut blob =
            dpapi::protect(plain, &entropy(target)).map_err(|e| os_err("암호화", target, e))?;
        if blob.len() > CRED_MAX_CREDENTIAL_BLOB_SIZE as usize {
            return Err(Error::Config(format!(
                "암호화한 값이 자격 증명 관리자 한도({CRED_MAX_CREDENTIAL_BLOB_SIZE}바이트)를 넘습니다"
            )));
        }
        write_blob(&mut name, &mut blob).map_err(|e| os_err("저장", target, e))
    }

    /// 없으면 [`Error::NotFound`]를 돌려줍니다.
    pub fn get(&self, target: &str) -> Result<SecretString> {
        let blob = read_blob(target)?;
        let plain = dpapi::unprotect(&blob, &entropy(target)).map_err(|e| {
            Error::Config(format!(
                "자격 증명 {target:?}를 복호화하지 못했습니다(다른 사용자 계정의 값이거나 손상된 값): {e}"
            ))
        })?;
        let text = std::str::from_utf8(&plain).map_err(|_| {
            Error::Config(format!("자격 증명 {target:?}가 UTF-8 문자열이 아닙니다"))
        })?;
        Ok(SecretString::from(text))
    }

    /// 없으면 [`Error::NotFound`]를 돌려줍니다.
    pub fn delete(&self, target: &str) -> Result<()> {
        let name = wide_target(target)?;
        let _lock = api_lock();
        // SAFETY: name은 NUL로 끝나는 UTF-16 문자열입니다.
        unsafe { CredDeleteW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, None) }
            .map_err(|e| os_err("삭제", target, e))
    }

    /// `prefix`로 시작하는 이 프로그램의 대상 이름을 정렬해 돌려줍니다.
    pub fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let mut filter = checked_utf16(prefix)?;
        filter.extend([u16::from(b'*'), 0]);
        let mut count = 0u32;
        let mut items: *mut *mut CREDENTIALW = ptr::null_mut();
        let found = {
            let _lock = api_lock();
            // SAFETY: filter는 NUL로 끝나고, 출력 포인터는 지역 변수입니다.
            unsafe { CredEnumerateW(PCWSTR(filter.as_ptr()), None, &mut count, &mut items) }
        };
        match found {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_NOT_FOUND.to_hresult() => return Ok(Vec::new()),
            Err(e) => return Err(os_err("목록 조회", prefix, e)),
        }
        let _free = CredBuf(items);
        // SAFETY: CredEnumerateW가 count개의 유효한 항목 포인터 배열을 돌려줍니다.
        let creds = unsafe { std::slice::from_raw_parts(items, count as usize) };
        let mut names: Vec<String> = creds
            .iter()
            .map(|&c| unsafe { &*c })
            .filter(|c| c.Type == CRED_TYPE_GENERIC && unsafe { pwstr(c.UserName) } == USER_NAME)
            .map(|c| unsafe { pwstr(c.TargetName) })
            .collect();
        names.sort();
        Ok(names)
    }
}

/// [`SecretSource`]를 환경 변수 또는 자격 증명 관리자에서 해석합니다.
#[derive(Debug, Default, Clone, Copy)]
pub struct Secrets;

impl SecretResolver for Secrets {
    fn resolve(&self, source: &SecretSource) -> Result<SecretString> {
        match source {
            SecretSource::Env { .. } => EnvSecrets.resolve(source),
            SecretSource::Credential { target } => CredentialStore.get(target),
        }
    }
}

/// CredReadW와 CredEnumerateW가 할당한 버퍼
struct CredBuf<T>(*mut T);

impl<T> Drop for CredBuf<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: 자격 증명 API가 할당한 버퍼를 한 번만 해제합니다.
            unsafe { CredFree(self.0.cast()) }
        }
    }
}

fn checked_utf16(name: &str) -> Result<Vec<u16>> {
    let units: Vec<u16> = name.encode_utf16().collect();
    if units.is_empty() || units.len() > MAX_TARGET_LEN || name.contains(['\0', '*']) {
        return Err(Error::Config(format!(
            "자격 증명 대상 {name:?}가 올바르지 않습니다(1~{MAX_TARGET_LEN}자, NUL과 * 제외)"
        )));
    }
    Ok(units)
}

fn wide_target(target: &str) -> Result<Vec<u16>> {
    let mut units = checked_utf16(target)?;
    units.push(0);
    Ok(units)
}

/// 자격 증명 관리자는 대상 이름의 대소문자를 구분하지 않으므로 소문자로 맞춰 묶습니다.
fn entropy(target: &str) -> Vec<u8> {
    target.to_lowercase().into_bytes()
}

/// # Safety
/// `p`는 null이거나 NUL로 끝나는 UTF-16 문자열이어야 합니다.
unsafe fn pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    String::from_utf16_lossy(unsafe { p.as_wide() })
}

fn write_blob(name: &mut [u16], blob: &mut [u8]) -> windows::core::Result<()> {
    let mut user: Vec<u16> = USER_NAME.encode_utf16().chain([0]).collect();
    let cred = CREDENTIALW {
        Type: CRED_TYPE_GENERIC,
        TargetName: PWSTR(name.as_mut_ptr()),
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        UserName: PWSTR(user.as_mut_ptr()),
        ..Default::default()
    };
    let _lock = api_lock();
    // SAFETY: 모든 포인터는 호출 동안 살아 있는 지역 버퍼를 가리킵니다.
    unsafe { CredWriteW(&cred, 0) }
}

/// 같은 프로세스에서 자격 증명 API 호출이 겹치면 성공한 삭제가 되살아나는 경우가 있어 순서대로 실행합니다.
fn api_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn read_blob(target: &str) -> Result<Vec<u8>> {
    let name = wide_target(target)?;
    let mut cred: *mut CREDENTIALW = ptr::null_mut();
    {
        let _lock = api_lock();
        // SAFETY: name은 NUL로 끝나고, 출력 포인터는 지역 변수입니다.
        unsafe { CredReadW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, None, &mut cred) }
    }
    .map_err(|e| os_err("읽기", target, e))?;
    let cred = CredBuf(cred);
    // SAFETY: 성공한 CredReadW는 유효한 CREDENTIALW를 돌려줍니다.
    let c = unsafe { &*cred.0 };
    if c.CredentialBlob.is_null() {
        return Ok(Vec::new());
    }
    // SAFETY: BLOB은 CredentialBlobSize 크기이며 cred가 살아 있는 동안 유효합니다.
    Ok(
        unsafe { std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize) }
            .to_vec(),
    )
}

/// Win32 오류를 공통 오류로 바꿉니다. 메시지에는 대상 이름만 넣습니다.
fn os_err(what: &str, target: &str, e: windows::core::Error) -> Error {
    if e.code() == ERROR_NOT_FOUND.to_hresult() {
        return Error::NotFound;
    }
    let code = e.code().0 as u32;
    let kind = if code >> 16 == 0x8007 {
        io::Error::from_raw_os_error((code & 0xFFFF) as i32).kind()
    } else {
        io::ErrorKind::Other
    };
    io::Error::new(kind, format!("자격 증명 {what} 실패({target:?}): {e}")).into()
}

#[cfg(test)]
mod tests;
