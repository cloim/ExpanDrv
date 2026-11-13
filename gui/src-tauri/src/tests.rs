use crate::{agent, commands};
use expandrv_core::config::Config;
use expandrv_ipc::{AgentStatus, DriveState, DriveStatus};

/// 실제 에이전트와 섞이지 않도록 없는 파이프 이름을 씁니다.
fn missing_pipe() -> String {
    format!(r"\\.\pipe\expandrv-gui-test-{}", std::process::id())
}

#[tokio::test]
async fn missing_agent_is_reported_plainly() {
    let pipe = missing_pipe();
    assert!(agent::connect(&pipe).await.unwrap().is_none());
    let err = commands::unmount(&pipe, "nas").await.unwrap_err();
    assert_eq!(err, "에이전트가 실행 중이 아닙니다");
    // 종료 요청은 실행 중이 아니면 할 일이 없습니다.
    assert!(!agent::stop(&pipe).await.unwrap());
}

#[tokio::test]
async fn start_reports_missing_cli() {
    let dir = tempfile::tempdir().unwrap();
    let err = agent::start(
        &missing_pipe(),
        &dir.path().join("config.toml"),
        &dir.path().join("expandrv.exe"),
        dir.path(),
    )
    .await
    .err()
    .unwrap();
    assert!(err.contains("CLI 실행 파일이 없습니다"), "{err}");
}

const CFG: &str = r#"
[[drive]]
id = "nas"
label = "NAS"
mount_point = "W:"
auto_mount = true
backend = { type = "local", root = 'C:\' }

[[drive]]
id = "bkt"
label = "Bucket"
mount_point = "B:"
backend = { type = "local", root = 'C:\' }
"#;

#[test]
fn merges_config_with_agent_status() {
    let cfg: Config = toml::from_str(CFG).unwrap();
    let live = |id: &str, state| DriveStatus {
        id: id.into(),
        label: id.into(),
        mount_point: "Q:".into(),
        backend: "webdav".into(),
        state,
        pending_uploads: 3,
        active_downloads: 2,
        uploading: 1,
        failed_uploads: 0,
    };
    let status = AgentStatus {
        drives: vec![
            live("nas", DriveState::Mounted),
            live("bkt", DriveState::Failed("인증 실패".into())),
            live("gone", DriveState::Mounted),
        ],
        ..AgentStatus::default()
    };
    let rows = commands::merge_status(Some(&cfg), &status);
    assert_eq!(rows.len(), 3);
    assert_eq!((rows[0].state, rows[0].pending_uploads), ("mounted", 3));
    assert!(rows[0].auto_mount && rows[0].configured);
    assert_eq!(rows[1].state, "failed");
    assert_eq!(rows[1].message.as_deref(), Some("인증 실패"));
    assert_eq!(rows[1].mount_point, "B:");
    assert!(!rows[2].configured);

    let idle = commands::merge_status(Some(&cfg), &AgentStatus::default());
    assert!(
        idle.iter()
            .all(|r| r.state == "unmounted" && r.pending_uploads == 0)
    );
}

#[test]
fn free_letters_skip_used_and_configured() {
    let cfg: Config = toml::from_str(CFG).unwrap();
    // C:, D:, Z: 사용 중
    let used = (1 << 2) | (1 << 3) | (1 << 25);
    let free = commands::free_letters(used, &cfg);
    assert_eq!(free.first().map(String::as_str), Some("Y:"));
    assert!(
        !free
            .iter()
            .any(|l| ["C:", "D:", "Z:", "W:", "B:"].contains(&l.as_str()))
    );
    // D~Z 23개 중 사용 중 D:, Z:와 설정의 W:를 뺍니다.
    assert_eq!(free.len(), 20);
}

#[test]
fn paths_detect_uses_dot_config() {
    let paths = commands::Paths::detect().unwrap();
    assert!(
        paths.config.ends_with(".config/ExpanDrv/config.toml")
            || paths.config.ends_with(r".config\ExpanDrv\config.toml")
    );
}

#[test]
fn notification_settings_save_keeps_drives_and_uses_camel_case() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let mut cfg: Config = toml::from_str(CFG).unwrap();
    cfg.version = expandrv_core::config::CONFIG_VERSION;
    cfg.save(&path).unwrap();

    let input: commands::NotificationSettings = serde_json::from_str(
        r#"{"mountStatus":true,"fileTransfers":false,"lockRecovery":true,"autoUpdater":false}"#,
    )
    .unwrap();
    commands::save_notifications(&path, input).unwrap();

    let saved = Config::load(&path).unwrap();
    assert_eq!(saved.drives, cfg.drives);
    assert!(saved.notifications.mount_status);
    assert!(!saved.notifications.file_transfers);
    assert!(saved.notifications.lock_recovery);
    assert!(!saved.notifications.auto_updater);

    let view =
        serde_json::to_value(commands::NotificationSettings::from(saved.notifications)).unwrap();
    assert_eq!(view["fileTransfers"], false);
    assert_eq!(view["autoUpdater"], false);
    assert!(
        serde_json::from_str::<commands::NotificationSettings>(r#"{"mountStatus":true}"#).is_err(),
        "화면은 네 항목을 모두 보내야 합니다"
    );
}
