use std::ffi::OsString;
use std::path::PathBuf;

/// 탐색기 실행 인자는 파일 경로만 받습니다. 경로 내용은 실행하거나 원격에 조회하지 않습니다.
#[derive(Default)]
pub struct Launch {
    pub config: Option<PathBuf>,
    pub manage_path: Option<PathBuf>,
    /// 로그인 자동 실행(시작 앱): 창 없이 에이전트만 띄우고 끝냅니다.
    pub autostart: bool,
}

impl Launch {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
        let mut args = args.into_iter();
        let mut launch = Self::default();
        while let Some(arg) = args.next() {
            let slot = match arg.to_str() {
                Some("--autostart") => {
                    if launch.autostart {
                        return Err("GUI 실행 인자가 중복되었습니다".into());
                    }
                    launch.autostart = true;
                    continue;
                }
                Some("--config") => &mut launch.config,
                Some("--manage-path") => &mut launch.manage_path,
                _ => return Err("지원하지 않는 GUI 실행 인자입니다".into()),
            };
            if slot.is_some() {
                return Err("GUI 실행 인자가 중복되었습니다".into());
            }
            let value = args
                .next()
                .filter(|v| !v.is_empty())
                .ok_or("GUI 실행 경로가 필요합니다")?;
            *slot = Some(PathBuf::from(value));
        }
        Ok(launch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gui_arguments_keep_paths_as_single_values() {
        let launch = Launch::parse(
            [
                "--config",
                r"C:\한글 설정\config.toml",
                "--manage-path",
                r"X:\\.",
            ]
            .map(OsString::from),
        )
        .unwrap();
        assert_eq!(
            launch.config.unwrap(),
            PathBuf::from(r"C:\한글 설정\config.toml")
        );
        assert_eq!(launch.manage_path.unwrap(), PathBuf::from(r"X:\\."));
        assert!(!launch.autostart);
        let auto =
            Launch::parse(["--config", r"C:\cfg\config.toml", "--autostart"].map(OsString::from))
                .unwrap();
        assert!(auto.autostart);
        assert_eq!(auto.config.unwrap(), PathBuf::from(r"C:\cfg\config.toml"));
        assert!(Launch::parse(["--autostart", "--autostart"].map(OsString::from)).is_err());
        assert!(Launch::parse(["--manage-path"].map(OsString::from)).is_err());
        assert!(Launch::parse(["--unknown", "path"].map(OsString::from)).is_err());
        assert!(Launch::parse(["--config", "a", "--config", "b"].map(OsString::from)).is_err());
    }
}
