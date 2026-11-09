//! 네트워크 드라이브(UNC) 등록 보조 기능.
//!
//! WinFsp 볼륨에 `\서버\공유` 접두사를 주면 네트워크 파일 시스템 장치로 만들어집니다.
//! 탐색기는 이 드라이브를 로컬 디스크가 아닌 네트워크 드라이브로 보고, 백신 실시간 검사와
//! 검색 색인, 셸 확장의 파일 내용 선읽기를 로컬 디스크보다 적게 수행합니다.

use windows::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RegDeleteKeyValueW, RegSetKeyValueW,
};
use windows::core::HSTRING;

/// 탐색기가 네트워크 드라이브 이름을 읽는 위치. 키 이름은 UNC의 `\`를 `#`으로 바꾼 값입니다.
const MOUNT_POINTS: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\MountPoints2";
const LABEL_VALUE: &str = "_LabelFromReg";

/// `X:` 형태인지 확인합니다. 폴더 마운트는 로컬 디스크로 유지합니다.
pub(crate) fn is_drive_letter(mount_point: &str) -> bool {
    let b = mount_point.trim_end_matches('\\').as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// UNC 이름에 안전한 문자만 남깁니다.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect()
}

/// 여러 Windows 사용자가 같은 드라이브 ID를 써도 겹치지 않도록 서버 이름에 사용자 이름을 넣습니다.
fn server_name(user: &str) -> String {
    let clean = sanitize(user);
    if !clean.is_empty() {
        return format!("ExpanDrv-{clean}");
    }
    // 한글처럼 ASCII가 아닌 사용자 이름은 해시로 구분합니다.
    let mut hash: u32 = 0x811c_9dc5;
    for b in user.as_bytes() {
        hash = (hash ^ u32::from(*b)).wrapping_mul(0x0100_0193);
    }
    format!("ExpanDrv-{hash:08x}")
}

/// WinFsp 볼륨 접두사 `\ExpanDrv-<사용자>\<공유>`
pub(crate) fn unc_prefix(share: &str) -> String {
    let user = std::env::var("USERNAME").unwrap_or_default();
    unc_prefix_for(&user, share)
}

fn unc_prefix_for(user: &str, share: &str) -> String {
    let share = match sanitize(share) {
        s if s.is_empty() => "drive".to_owned(),
        s => s,
    };
    format!(r"\{}\{share}", server_name(user))
}

fn label_key(prefix: &str) -> HSTRING {
    // `\서버\공유`는 `##서버#공유`가 됩니다.
    let name = format!("#{}", prefix.replace('\\', "#"));
    HSTRING::from(format!(r"{MOUNT_POINTS}\{name}"))
}

/// 탐색기의 네트워크 드라이브 표시 이름을 볼륨 레이블로 맞춥니다. 실패해도 마운트는 유지합니다.
pub(crate) fn set_explorer_label(prefix: &str, label: &str) {
    if label.is_empty() {
        return;
    }
    let wide: Vec<u16> = label.encode_utf16().chain(Some(0)).collect();
    let status = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &label_key(prefix),
            &HSTRING::from(LABEL_VALUE),
            REG_SZ.0,
            Some(wide.as_ptr().cast()),
            (wide.len() * 2) as u32,
        )
    };
    if status.is_err() {
        tracing::debug!(prefix, ?status, "탐색기 드라이브 이름 설정 실패");
    }
}

pub(crate) fn clear_explorer_label(prefix: &str) {
    unsafe {
        let _ = RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            &label_key(prefix),
            &HSTRING::from(LABEL_VALUE),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_letters_only() {
        assert!(is_drive_letter("N:"));
        assert!(is_drive_letter("n:\\"));
        assert!(!is_drive_letter(r"C:\Mount"));
        assert!(!is_drive_letter("1:"));
    }

    #[test]
    fn prefix_is_unc_safe_and_unique_per_user() {
        assert_eq!(unc_prefix_for("cloim", "DS920"), r"\ExpanDrv-cloim\DS920");
        assert_eq!(
            unc_prefix_for("John Doe", "my drive"),
            r"\ExpanDrv-JohnDoe\mydrive"
        );
        let a = unc_prefix_for("홍길동", "x");
        let b = unc_prefix_for("김철수", "x");
        assert!(a.starts_with(r"\ExpanDrv-") && a.ends_with(r"\x"));
        assert_ne!(a, b);
        assert_eq!(unc_prefix_for("u", ""), r"\ExpanDrv-u\drive");
    }

    #[test]
    fn label_key_matches_explorer_format() {
        assert_eq!(
            label_key(r"\ExpanDrv-u\DS920").to_string(),
            format!(r"{MOUNT_POINTS}\##ExpanDrv-u#DS920")
        );
    }
}
