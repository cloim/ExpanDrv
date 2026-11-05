//! DPAPI(현재 사용자 범위) 암호화와 복호화

use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::core::PCWSTR;
use zeroize::{Zeroize, Zeroizing};

/// DPAPI가 LocalAlloc으로 돌려준 버퍼. 해제하기 전에 내용을 지웁니다.
struct LocalBlob(CRYPT_INTEGER_BLOB);

impl LocalBlob {
    fn as_slice(&self) -> &[u8] {
        if self.0.pbData.is_null() {
            return &[];
        }
        // SAFETY: DPAPI가 cbData 크기로 할당해 돌려준 버퍼입니다.
        unsafe { std::slice::from_raw_parts(self.0.pbData, self.0.cbData as usize) }
    }
}

impl Drop for LocalBlob {
    fn drop(&mut self) {
        if self.0.pbData.is_null() {
            return;
        }
        // SAFETY: 위와 같은 버퍼이며, 지운 뒤 한 번만 해제합니다.
        unsafe {
            std::slice::from_raw_parts_mut(self.0.pbData, self.0.cbData as usize).zeroize();
            LocalFree(Some(HLOCAL(self.0.pbData.cast())));
        }
    }
}

/// 입력 전용 BLOB. DPAPI는 입력 버퍼를 고치지 않습니다.
fn input(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(data.len()).expect("DPAPI 입력 크기는 호출 전에 제한합니다"),
        pbData: data.as_ptr().cast_mut(),
    }
}

/// `entropy`가 다르면 복호화할 수 없으므로 암호문을 특정 대상에 묶을 때 씁니다.
pub(crate) fn protect(plain: &[u8], entropy: &[u8]) -> windows::core::Result<Vec<u8>> {
    let (data, entropy) = (input(plain), input(entropy));
    let mut out = LocalBlob(CRYPT_INTEGER_BLOB::default());
    // SAFETY: 입력 BLOB은 호출 동안 살아 있는 슬라이스를 가리킵니다.
    unsafe {
        CryptProtectData(
            &data,
            PCWSTR::null(),
            Some(&raw const entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out.0,
        )?;
    }
    Ok(out.as_slice().to_vec())
}

/// 복호화한 평문은 드롭할 때 지워집니다.
pub(crate) fn unprotect(
    cipher: &[u8],
    entropy: &[u8],
) -> windows::core::Result<Zeroizing<Vec<u8>>> {
    let (data, entropy) = (input(cipher), input(entropy));
    let mut out = LocalBlob(CRYPT_INTEGER_BLOB::default());
    // SAFETY: protect와 같습니다.
    unsafe {
        CryptUnprotectData(
            &data,
            None,
            Some(&raw const entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out.0,
        )?;
    }
    Ok(Zeroizing::new(out.as_slice().to_vec()))
}
