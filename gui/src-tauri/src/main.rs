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
