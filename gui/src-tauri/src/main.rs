//! ExpanDrv 데스크톱 GUI. 관리자 권한 없이 실행하고, 화면은 등록한 명령만 호출합니다.
// 릴리스 빌드에서는 콘솔 창을 띄우지 않습니다.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent;
mod commands;
mod drives;
mod launch;

use commands::{AppState, Paths};

fn main() {
    let launch = match launch::Launch::parse(std::env::args_os().skip(1)) {
        Ok(launch) => launch,
        Err(e) => {
            eprintln!("오류: {e}");
            std::process::exit(1);
        }
    };
    let mut paths = match Paths::detect() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("오류: {e}");
            std::process::exit(1);
        }
    };
    if let Some(config) = &launch.config {
        paths.config = expandrv_core::config::expand_tilde(config);
    }
    if launch.autostart {
        // 시작 앱: 창을 만들지 않고 에이전트만 띄운 뒤 끝냅니다.
        let r = tauri::async_runtime::block_on(agent::launch_autostart(
            &paths.pipe,
            &paths.config,
            &paths.cli,
        ));
        if let Err(e) = r {
            log_autostart_error(&paths.log_dir, &e);
            std::process::exit(1);
        }
        return;
    }
    tauri::Builder::default()
        .manage(launch)
        .manage(AppState {
            paths,
            config_lock: Default::default(),
        })
        .invoke_handler(tauri::generate_handler![
            commands::agent_status,
            commands::mount_drive,
            commands::mount_all,
            commands::unmount_drive,
            commands::get_drive,
            commands::add_drive,
            commands::update_drive,
            commands::update_connection,
            commands::set_drive_secret,
            commands::delete_drive,
            commands::test_connection,
            commands::check_drive,
            commands::free_drive_letters,
            commands::get_settings,
            commands::set_explorer_menu,
            commands::launch_drive,
            commands::set_autostart,
            commands::set_auto_lock,
            commands::set_notifications,
            commands::stop_agent,
            commands::lock_drives,
            commands::unlock_drives,
            commands::set_lock_pin,
        ])
        // 앱 자산 밖의 주소로 이동하지 않습니다(외부 링크 차단).
        .plugin(navigation_guard())
        .run(tauri::generate_context!())
        .expect("ExpanDrv GUI 실행 실패");
}

/// 콘솔이 없으므로 자동 실행 실패를 로그 폴더에 남깁니다.
fn log_autostart_error(log_dir: &std::path::Path, message: &str) {
    use std::io::Write;
    let _ = std::fs::create_dir_all(log_dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("autostart.log"))
    {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        let _ = writeln!(f, "[{secs}] 자동 실행 실패: {message}");
    }
}

fn navigation_guard<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    tauri::plugin::Builder::new("navigation-guard")
        .on_navigation(|_, url| {
            matches!(url.scheme(), "tauri" | "http" | "https")
                && matches!(url.host_str(), Some("localhost" | "tauri.localhost"))
        })
        .build()
}

#[cfg(test)]
mod tests;
