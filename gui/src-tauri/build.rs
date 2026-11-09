// 화면이 호출할 수 있는 명령 목록. 여기 없는 명령은 권한이 생기지 않아 호출할 수 없습니다.
const COMMANDS: &[&str] = &[
    "agent_status",
    "mount_drive",
    "mount_all",
    "unmount_drive",
    "get_drive",
    "add_drive",
    "update_drive",
    "update_connection",
    "set_drive_secret",
    "delete_drive",
    "test_connection",
    "check_drive",
    "free_drive_letters",
    "get_settings",
    "set_explorer_menu",
    "launch_drive",
    "set_autostart",
    "set_auto_lock",
    "stop_agent",
    "lock_drives",
    "unlock_drives",
    "set_lock_pin",
];

fn main() {
    // generate_context!는 frontendDist가 있어야 컴파일됩니다. `npm run build` 전에도
    // `cargo build --workspace`가 되도록 안내 페이지를 둡니다(gui/dist는 git에서 제외).
    let dist = std::path::Path::new("../dist");
    if !dist.join("index.html").exists() {
        std::fs::create_dir_all(dist).expect("gui/dist 생성");
        std::fs::write(dist.join("index.html"), PLACEHOLDER).expect("gui/dist/index.html 생성");
    }
    println!("cargo:rerun-if-changed=../dist");

    // 관리자 권한을 요구하지 않도록 asInvoker를 명시합니다.
    println!("cargo:rerun-if-changed=windows-app.manifest");
    let windows =
        tauri_build::WindowsAttributes::new().app_manifest(include_str!("windows-app.manifest"));
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .windows_attributes(windows)
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("tauri-build 실패");
}

const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="ko">
<head><meta charset="utf-8"><title>ExpanDrv</title></head>
<body>
<p>화면 자산이 없습니다. <code>gui</code> 폴더에서 <code>npm run build</code>를 실행한 뒤 다시 빌드하세요.</p>
</body>
</html>
"#;
