//! 현재 사용자 탐색기 메뉴. 실행 파일을 직접 호출하며 셸 명령문을 만들지 않습니다.
use expandrv_core::config::Config;
use expandrv_core::{Error, Result};
use std::path::Path;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, REG_SZ, RRF_RT_REG_SZ, RegCloseKey, RegDeleteTreeW,
    RegGetValueW, RegOpenKeyExW, RegSetKeyValueW,
};
use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
use windows::core::HSTRING;

const CLASSES: &str = r"Software\Classes";
const OWNER: &str = "ExpanDrv.shell.v1";
const CONTEXTS: [(&str, &str); 3] = [
    (r"Drive\shell", "%1"),
    (r"Directory\shell", "%1"),
    (r"Directory\Background\shell", "%V"),
];

fn win(error: WIN32_ERROR) -> Error {
    Error::Io(std::io::Error::from_raw_os_error(error.0 as i32))
}

fn read(key: &str, name: &str) -> Result<Option<String>> {
    let mut size = 0;
    let error = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(key),
            &HSTRING::from(name),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if error == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if error.is_err() {
        return Err(win(error));
    }
    let mut data = vec![0u16; size as usize / 2 + 1];
    let error = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(key),
            &HSTRING::from(name),
            RRF_RT_REG_SZ,
            None,
            Some(data.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if error.is_err() {
        return Err(win(error));
    }
    let len = data.iter().position(|&c| c == 0).unwrap_or(data.len());
    Ok(Some(String::from_utf16_lossy(&data[..len])))
}

fn exists(key: &str) -> Result<bool> {
    let mut handle = HKEY::default();
    let error = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            &HSTRING::from(key),
            None,
            KEY_READ,
            &mut handle,
        )
    };
    if error == ERROR_FILE_NOT_FOUND {
        return Ok(false);
    }
    if error.is_err() {
        return Err(win(error));
    }
    unsafe {
        let _ = RegCloseKey(handle);
    }
    Ok(true)
}

fn write(key: &str, name: &str, value: &str) -> Result<()> {
    let data: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    let error = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            &HSTRING::from(key),
            &HSTRING::from(name),
            REG_SZ.0,
            Some(data.as_ptr().cast()),
            (data.len() * 2) as u32,
        )
    };
    if error.is_err() {
        return Err(win(error));
    }
    Ok(())
}

/// 전용 키 6개만 다룹니다. 테스트에서는 실제 Classes 트리와 분리합니다.
struct Registration<'a> {
    root: &'a str,
}

impl Registration<'_> {
    fn keys(&self) -> Vec<(String, &str, bool)> {
        CONTEXTS
            .iter()
            .flat_map(|(context, placeholder)| {
                [
                    (
                        format!(r"{}\{context}\ExpanDrv.Open", self.root),
                        *placeholder,
                        false,
                    ),
                    (
                        format!(r"{}\{context}\ExpanDrv.Manage", self.root),
                        *placeholder,
                        true,
                    ),
                ]
            })
            .collect()
    }

    fn install(&self, gui: &Path, config: &Path) -> Result<()> {
        if !gui.is_file() {
            return Err(Error::Config(format!(
                "GUI 실행 파일이 없습니다: {}",
                gui.display()
            )));
        }
        let gui = std::path::absolute(gui)?;
        let config = std::path::absolute(config)?;
        let gui = gui
            .to_str()
            .ok_or_else(|| Error::Config("GUI 경로 형식 오류".into()))?;
        let config = config
            .to_str()
            .ok_or_else(|| Error::Config("설정 경로 형식 오류".into()))?;
        if [gui, config]
            .iter()
            .any(|s| s.contains(['"', '\0', '\r', '\n']))
        {
            return Err(Error::Config(
                "실행 경로에 사용할 수 없는 문자가 있습니다".into(),
            ));
        }
        let keys = self.keys();
        // 쓰기 전에 모든 대상의 소유권을 확인합니다.
        for (key, _, _) in &keys {
            let owner = read(key, "ExpanDrvOwner")?;
            if owner.as_deref().is_some_and(|value| value != OWNER)
                || (owner.is_none() && exists(key)?)
            {
                return Err(Error::Config(format!(
                    "다른 프로그램의 메뉴 등록을 보존합니다: {key}"
                )));
            }
        }
        for (key, placeholder, manage) in keys {
            write(&key, "ExpanDrvOwner", OWNER)?;
            write(
                &key,
                "",
                if manage {
                    "ExpanDrv에서 이 드라이브 관리"
                } else {
                    "ExpanDrv 열기"
                },
            )?;
            write(&key, "Icon", &format!("\"{gui}\",0"))?;
            write(&key, "MultiSelectModel", "Single")?;
            write(&key, "NoWorkingDirectory", "")?;
            // 끝의 \.은 드라이브 루트의 마지막 역슬래시가 닫는 따옴표를 이스케이프하는 것을 막습니다.
            let suffix = if manage {
                format!(" --manage-path \"{placeholder}\\.\"")
            } else {
                String::new()
            };
            write(
                &format!(r"{key}\command"),
                "",
                &format!("\"{gui}\" --config \"{config}\"{suffix}"),
            )?;
        }
        Ok(())
    }

    fn enabled(&self) -> Result<bool> {
        for (key, _, _) in self.keys() {
            if read(&key, "ExpanDrvOwner")?.as_deref() == Some(OWNER) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn uninstall(&self) -> Result<()> {
        self.uninstall_matching(None)
    }

    fn uninstall_matching(&self, gui: Option<&Path>) -> Result<()> {
        let prefix = gui
            .map(|path| {
                std::path::absolute(path)
                    .map(|path| format!("\"{}\" ", path.display()).to_lowercase())
            })
            .transpose()?;
        for (key, _, _) in self.keys() {
            if read(&key, "ExpanDrvOwner")?.as_deref() != Some(OWNER) {
                continue;
            }
            if let Some(prefix) = &prefix {
                let command = read(&format!(r"{key}\command"), "")?;
                if !command.is_some_and(|command| command.to_lowercase().starts_with(prefix)) {
                    continue;
                }
            }
            let error = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(key)) };
            if error.is_err() && error != ERROR_FILE_NOT_FOUND {
                return Err(win(error));
            }
        }
        Ok(())
    }
}

pub fn install(gui: &Path, config: &Path) -> Result<()> {
    Registration { root: CLASSES }.install(gui, config)?;
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    Registration { root: CLASSES }.uninstall()?;
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
    }
    Ok(())
}

/// 설치 제거 시 해당 실행 파일을 가리키는 메뉴만 해제합니다.
pub fn uninstall_for(gui: &Path) -> Result<()> {
    Registration { root: CLASSES }.uninstall_matching(Some(gui))?;
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None);
    }
    Ok(())
}

pub fn enabled() -> Result<bool> {
    Registration { root: CLASSES }.enabled()
}

/// 원격 경로에 접근하지 않고 로컬 절대 경로만 정규화합니다.
fn normalized(path: &str) -> Option<String> {
    let path = path.replace('/', "\\");
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
    let bytes = path.as_bytes();
    if bytes.len() < 2
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || (bytes.len() > 2 && bytes[2] != b'\\')
        || path.contains('\0')
    {
        return None;
    }
    let mut parts = Vec::new();
    for part in path[2..].split('\\') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    Some(format!("{}:\\{}", &path[..1], parts.join("\\")).to_lowercase())
}

/// 경계가 일치하는 가장 긴 마운트 경로를 선택합니다.
pub fn drive_for_path<'a>(path: &Path, config: &'a Config) -> Option<&'a str> {
    let path = normalized(path.to_str()?)?;
    config
        .drives
        .iter()
        .filter_map(|drive| {
            let mount = normalized(&drive.mount_point)?;
            let prefix = if mount.ends_with('\\') {
                mount.clone()
            } else {
                format!("{mount}\\")
            };
            (path == mount || path.starts_with(&prefix)).then_some((mount.len(), drive.id.as_str()))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, id)| id)
}

#[cfg(test)]
mod tests;
