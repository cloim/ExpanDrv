//! 에이전트 수명과 함께 동작하는 Windows 알림 영역.
use expandrv_ipc::{AgentStatus, DriveState};
use expandrv_vfs::{TransferEvent, Vfs};
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::UI::{Shell::*, WindowsAndMessaging::*};
use windows::core::w;

const CALLBACK: u32 = WM_APP + 1;
const ACTION: u32 = WM_APP + 2;
const RESET: u32 = WM_APP + 3;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateStatus {
    Idle,
    Checking,
    Available(crate::updater::UpdateCandidate),
    Downloading,
    ReadyToInstall {
        installer_path: std::path::PathBuf,
        version: String,
        install_kind: crate::updater::InstallKind,
    },
    Installing {
        version: String,
    },
}

/// 업데이트 상태에 따른 트레이 메뉴 라벨, 활성화 여부(클릭 가능 여부) 및 메뉴 ID를 반환합니다.
pub fn menu_item_for_update_status(status: &UpdateStatus) -> (String, bool, u32) {
    match status {
        UpdateStatus::Idle => ("업데이트 확인".into(), true, 10),
        UpdateStatus::Checking => ("업데이트 확인 중...".into(), false, 10),
        UpdateStatus::Available(c) => (format!("v{} 다운로드 및 설치", c.version_str), true, 11),
        UpdateStatus::Downloading => ("업데이트 다운로드 중...".into(), false, 10),
        UpdateStatus::ReadyToInstall { version, .. } => (format!("v{version} 지금 설치"), true, 12),
        UpdateStatus::Installing { version } => (format!("v{version} 설치 진행 중..."), false, 10),
    }
}

enum Update {
    Status(AgentStatus),
    Notice(String, String),
    Shutdown(mpsc::Sender<()>),
    Updater(UpdateStatus),
    #[cfg(test)]
    SessionEvent(u32),
}

#[derive(Clone)]
pub struct Tray(mpsc::SyncSender<Update>);

impl Tray {
    pub fn start(
        rt: tokio::runtime::Handle,
        stop: Arc<Notify>,
        lock: Arc<Notify>,
        session: Arc<Notify>,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(128);
        let tx_clone = tx.clone();
        std::thread::Builder::new()
            .name("expandrv-tray".into())
            .spawn(move || {
                if let Err(e) = unsafe { run(rx, tx_clone, rt, stop, lock, session) } {
                    tracing::warn!(error = %e, "트레이를 시작하지 못했습니다");
                }
            })?;
        Ok(Self(tx))
    }

    pub fn shutdown(&self) -> bool {
        let (tx, rx) = mpsc::channel();
        self.0.try_send(Update::Shutdown(tx)).is_ok()
            && rx.recv_timeout(Duration::from_secs(1)).is_ok()
    }

    pub fn status(&self, status: AgentStatus) {
        let _ = self.0.try_send(Update::Status(status));
    }

    pub fn notice(&self, title: &str, body: &str) {
        let _ = self.0.try_send(Update::Notice(title.into(), body.into()));
    }

    pub fn transfers(&self, id: &str, vfs: &Vfs) {
        for event in vfs.take_transfer_events() {
            let (title, path) = match event {
                TransferEvent::Uploaded(p) => ("업로드 완료", p),
                TransferEvent::Conflict(p) => ("충돌 사본 생성", p),
            };
            let _ = self
                .0
                .try_send(Update::Notice(title.into(), format!("{id}: {path}")));
        }
    }
}

fn wide<const N: usize>(target: &mut [u16; N], text: &str) {
    target.fill(0);
    let mut i = 0;
    for c in text.chars() {
        let mut buf = [0; 2];
        let encoded = c.encode_utf16(&mut buf);
        if i + encoded.len() >= N {
            break;
        }
        target[i..i + encoded.len()].copy_from_slice(encoded);
        i += encoded.len();
    }
}

fn tooltip(status: &AgentStatus) -> String {
    let mounted = status
        .drives
        .iter()
        .filter(|d| d.state == DriveState::Mounted)
        .count();
    let failed = status
        .drives
        .iter()
        .filter(|d| matches!(d.state, DriveState::Failed(_)))
        .count();
    let pending: u64 = status.drives.iter().map(|d| d.pending_uploads).sum();
    if status.locking {
        "ExpanDrv | 잠그는 중".into()
    } else if status.locked {
        "ExpanDrv | 잠김".into()
    } else {
        format!("ExpanDrv | 마운트 {mounted}개 | 업로드 대기 {pending}개 | 오류 {failed}개")
    }
}

fn open_gui() {
    let result = std::env::current_exe().and_then(|exe| {
        std::process::Command::new(exe.with_file_name("expandrv-gui.exe"))
            .creation_flags(0x0800_0000)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
    });
    if let Err(e) = result {
        tracing::warn!(error = %e, "GUI 실행 실패");
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_WTSSESSION_CHANGE {
            if wp.0 as u32 == WTS_SESSION_LOCK {
                let notify = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Notify;
                if let Some(notify) = notify.as_ref() {
                    notify.notify_one();
                }
            }
            return LRESULT(0);
        }
        if msg == WM_NCDESTROY {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        }
        if msg == CALLBACK {
            let _ = PostMessageW(Some(hwnd), ACTION, wp, lp);
            return LRESULT(0);
        }
        if msg == RegisterWindowMessageW(w!("TaskbarCreated")) {
            let _ = PostMessageW(Some(hwnd), RESET, wp, lp);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

unsafe fn run(
    rx: mpsc::Receiver<Update>,
    tx_self: mpsc::SyncSender<Update>,
    rt: tokio::runtime::Handle,
    stop: Arc<Notify>,
    lock: Arc<Notify>,
    session: Arc<Notify>,
) -> windows::core::Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("ExpanDrvTray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class,
            w!("ExpanDrv"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        // Notify의 Arc는 창 파괴까지 살아 있으므로 WindowProc의 포인터도 유효합니다.
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Arc::as_ptr(&session) as isize);
        let mut session_events = SessionNotifications {
            hwnd,
            registered: false,
            retry_at: Instant::now(),
        };
        session_events.register();
        let resource = include_bytes!("../../../gui/src-tauri/icons/icon.ico");
        let length = u32::from_le_bytes(resource[14..18].try_into().unwrap()) as usize;
        let offset = u32::from_le_bytes(resource[18..22].try_into().unwrap()) as usize;
        let app_icon = CreateIconFromResourceEx(
            &resource[offset..offset + length],
            true,
            0x0003_0000,
            32,
            32,
            IMAGE_FLAGS::default(),
        )?;
        let mut icon = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: CALLBACK,
            hIcon: app_icon,
            ..Default::default()
        };
        wide(&mut icon.szTip, "ExpanDrv | 에이전트 실행 중");
        let _ = Shell_NotifyIconW(NIM_ADD, &icon);
        let mut states = HashMap::new();
        let mut update_status = UpdateStatus::Idle;
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        loop {
            if !session_events.registered
                && session_events.retry_at.elapsed() >= Duration::from_secs(5)
            {
                session_events.register();
            }
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == ACTION {
                    match msg.lParam.0 as u32 {
                        WM_LBUTTONDBLCLK => open_gui(),
                        WM_RBUTTONUP | WM_CONTEXTMENU => {
                            let menu = CreatePopupMenu()?;
                            let _ = AppendMenuW(menu, MF_STRING, 1, w!("ExpanDrv 열기"));
                            let _ = AppendMenuW(menu, MF_STRING, 3, w!("드라이브 잠금"));
                            let _ =
                                AppendMenuW(menu, MF_STRING, 2, w!("모든 드라이브 해제 후 종료"));
                            let _ =
                                AppendMenuW(menu, MF_SEPARATOR, 0, windows::core::PCWSTR::null());

                            let mut menu_text_buf = [0u16; 64];
                            let (label, enabled, item_id) =
                                menu_item_for_update_status(&update_status);
                            wide(&mut menu_text_buf, &label);
                            let flags = if enabled {
                                MF_STRING
                            } else {
                                MF_STRING | MF_GRAYED
                            };
                            let _ = AppendMenuW(
                                menu,
                                flags,
                                item_id as usize,
                                windows::core::PCWSTR(menu_text_buf.as_ptr()),
                            );

                            let mut point = POINT::default();
                            let _ = GetCursorPos(&mut point);
                            let _ = SetForegroundWindow(hwnd);
                            let choice = TrackPopupMenu(
                                menu,
                                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                                point.x,
                                point.y,
                                None,
                                hwnd,
                                None,
                            )
                            .0;
                            let _ = DestroyMenu(menu);
                            match choice {
                                1 => open_gui(),
                                2 => stop.notify_one(),
                                3 => lock.notify_one(),
                                10 => {
                                    if update_status == UpdateStatus::Idle {
                                        let install_kind =
                                            crate::updater::detect_current_install_kind();
                                        if install_kind == crate::updater::InstallKind::Unknown {
                                            balloon(
                                                &mut icon,
                                                "업데이트 불가",
                                                "현재 설치 형식을 식별할 수 없어 자동 업데이트를 지원하지 않습니다 (개발 빌드 또는 미등록 경로).",
                                            );
                                        } else {
                                            update_status = UpdateStatus::Checking;
                                            balloon(
                                                &mut icon,
                                                "업데이트 확인",
                                                "최신 버전을 확인하고 있습니다...",
                                            );
                                            let tx = tx_self.clone();
                                            rt.spawn(async move {
                                                let client =
                                                    match crate::updater::build_http_client() {
                                                        Ok(c) => c,
                                                        Err(e) => {
                                                            let _ = tx.try_send(Update::Notice(
                                                                "업데이트 오류".into(),
                                                                format!(
                                                                    "HTTP 클라이언트 생성 실패: {e}"
                                                                ),
                                                            ));
                                                            let _ = tx.try_send(Update::Updater(
                                                                UpdateStatus::Idle,
                                                            ));
                                                            return;
                                                        }
                                                    };
                                                let current_ver_str = env!("CARGO_PKG_VERSION");
                                                let current_ver = match crate::updater::parse_version(
                                                    current_ver_str,
                                                ) {
                                                    Some(v) => v,
                                                    None => {
                                                        let _ = tx.try_send(Update::Notice(
                                                            "업데이트 오류".into(),
                                                            "현재 버전 형식을 확인할 수 없습니다"
                                                                .into(),
                                                        ));
                                                        let _ = tx.try_send(Update::Updater(
                                                            UpdateStatus::Idle,
                                                        ));
                                                        return;
                                                    }
                                                };
                                                match crate::updater::fetch_releases(
                                                    &client,
                                                    crate::updater::DEFAULT_REPO_OWNER,
                                                    crate::updater::DEFAULT_REPO_NAME,
                                                )
                                                .await
                                                {
                                                    Ok(releases) => {
                                                        match crate::updater::find_latest_update(
                                                            &releases,
                                                            &current_ver,
                                                            install_kind,
                                                        ) {
                                                            Ok(Some(candidate)) => {
                                                                let body = format!(
                                                                    "새 버전 v{} ({:?})을 사용할 수 있습니다. 트레이 메뉴에서 다운로드하세요.",
                                                                    candidate.version_str, candidate.install_kind
                                                                );
                                                                let _ = tx.try_send(Update::Notice(
                                                                    "업데이트 가능".into(),
                                                                    body,
                                                                ));
                                                                let _ = tx.try_send(Update::Updater(
                                                                    UpdateStatus::Available(
                                                                        candidate,
                                                                    ),
                                                                ));
                                                            }
                                                            Ok(None) => {
                                                                let body = format!(
                                                                    "현재 최신 버전(v{})을 사용하고 있습니다",
                                                                    current_ver_str
                                                                );
                                                                let _ = tx.try_send(Update::Notice(
                                                                    "최신 버전".into(),
                                                                    body,
                                                                ));
                                                                let _ = tx.try_send(Update::Updater(
                                                                    UpdateStatus::Idle,
                                                                ));
                                                            }
                                                            Err(e) => {
                                                                let _ = tx.try_send(Update::Notice(
                                                                    "업데이트 확인 실패".into(),
                                                                    e.to_string(),
                                                                ));
                                                                let _ = tx.try_send(Update::Updater(
                                                                    UpdateStatus::Idle,
                                                                ));
                                                            }
                                                        }
                                                    }
                                                    Err(e) => {
                                                        let _ = tx.try_send(Update::Notice(
                                                            "업데이트 확인 실패".into(),
                                                            e.to_string(),
                                                        ));
                                                        let _ = tx.try_send(Update::Updater(
                                                            UpdateStatus::Idle,
                                                        ));
                                                    }
                                                }
                                            });
                                        }
                                    }
                                }
                                11 => {
                                    if let UpdateStatus::Available(candidate) = &update_status {
                                        let candidate = candidate.clone();
                                        update_status = UpdateStatus::Downloading;
                                        balloon(
                                            &mut icon,
                                            "다운로드 시작",
                                            &format!(
                                                "ExpanDrv v{} 설치 패키지를 다운로드하고 있습니다...",
                                                candidate.version_str
                                            ),
                                        );
                                        let tx = tx_self.clone();
                                        rt.spawn(async move {
                                            let client = match crate::updater::build_http_client() {
                                                Ok(c) => c,
                                                Err(e) => {
                                                    let _ = tx.try_send(Update::Notice(
                                                        "다운로드 오류".into(),
                                                        e.to_string(),
                                                    ));
                                                    let _ = tx.try_send(Update::Updater(
                                                        UpdateStatus::Idle,
                                                    ));
                                                    return;
                                                }
                                            };
                                            let manifest = match crate::updater::fetch_manifest(
                                                &client,
                                                &candidate.manifest_asset.url,
                                            )
                                            .await
                                            {
                                                Ok(m) => m,
                                                Err(e) => {
                                                    let _ = tx.try_send(Update::Notice(
                                                        "매니페스트 확인 실패".into(),
                                                        e.to_string(),
                                                    ));
                                                    let _ = tx.try_send(Update::Updater(
                                                        UpdateStatus::Idle,
                                                    ));
                                                    return;
                                                }
                                            };
                                            let artifact = match crate::updater::verify_manifest(
                                                &manifest,
                                                &candidate,
                                            ) {
                                                Ok(a) => a.clone(),
                                                Err(e) => {
                                                    let _ = tx.try_send(Update::Notice(
                                                        "매니페스트 검증 실패".into(),
                                                        e.to_string(),
                                                    ));
                                                    let _ = tx.try_send(Update::Updater(
                                                        UpdateStatus::Idle,
                                                    ));
                                                    return;
                                                }
                                            };
                                            match crate::updater::download_and_verify_setup(
                                                &client,
                                                &candidate,
                                                &artifact,
                                            )
                                            .await
                                            {
                                                Ok(path) => {
                                                    let body = format!(
                                                        "v{} 설치 패키지가 준비되었습니다. 메뉴에서 설치를 진행하세요.",
                                                        candidate.version_str
                                                    );
                                                    let _ = tx.try_send(Update::Notice(
                                                        "다운로드 완료".into(),
                                                        body,
                                                    ));
                                                    let _ = tx.try_send(Update::Updater(
                                                        UpdateStatus::ReadyToInstall {
                                                            installer_path: path,
                                                            version: candidate.version_str,
                                                            install_kind: candidate.install_kind,
                                                        },
                                                    ));
                                                }
                                                Err(e) => {
                                                    let _ = tx.try_send(Update::Notice(
                                                        "다운로드 검증 실패".into(),
                                                        e.to_string(),
                                                    ));
                                                    let _ = tx.try_send(Update::Updater(
                                                        UpdateStatus::Idle,
                                                    ));
                                                }
                                            }
                                        });
                                    }
                                }
                                12 => {
                                    if let UpdateStatus::ReadyToInstall {
                                        installer_path,
                                        version,
                                        install_kind,
                                    } = &update_status
                                    {
                                        let mut title_buf = [0u16; 64];
                                        wide(&mut title_buf, "ExpanDrv 업데이트");
                                        let prompt = format!(
                                            "ExpanDrv v{} 업데이트 설치 프로그램을 시작합니다.\n인스톨러가 시작되면 기존 에이전트와 미완료 업로드를 정상 정리한 뒤 설치를 진행합니다.\n계속하시겠습니까?",
                                            version
                                        );
                                        let mut prompt_buf = [0u16; 256];
                                        wide(&mut prompt_buf, &prompt);
                                        let ret = MessageBoxW(
                                            Some(hwnd),
                                            windows::core::PCWSTR(prompt_buf.as_ptr()),
                                            windows::core::PCWSTR(title_buf.as_ptr()),
                                            MB_YESNO | MB_ICONINFORMATION,
                                        );
                                        if ret == IDYES {
                                            let installer_path_clone = installer_path.clone();
                                            let version_clone = version.clone();
                                            let install_kind_val = *install_kind;

                                            // 1. 중복 실행 차단을 위해 즉시 Installing 상태로 전이합니다.
                                            update_status = UpdateStatus::Installing {
                                                version: version_clone.clone(),
                                            };

                                            // 2. 인스톨러 프로세스를 시작합니다.
                                            // 인스톨러의 사전 실행 훅이 스스로 `expandrv.exe stop`을 호출하여
                                            // 미완료 업로드 대기와 드라이브 해제 결과를 검증하도록 보존하며,
                                            // 에이전트가 사전에 프로세스를 닫아버려 경합하거나 관측을 방해하지 않습니다.
                                            match crate::updater::launch_installer(
                                                &installer_path_clone,
                                                install_kind_val,
                                            ) {
                                                Ok(safe_handle) => {
                                                    balloon(
                                                        &mut icon,
                                                        "설치 시작",
                                                        &format!(
                                                            "v{} 설치 프로그램을 시작했습니다.",
                                                            version_clone
                                                        ),
                                                    );
                                                    // 3. 인스톨러 프로세스 종료를 비동기 감시하여, 사용자가 마법사에서 취소하거나
                                                    // 설치가 중단되어 에이전트가 살아있는 경우 ReadyToInstall 상태로 복구합니다.
                                                    // Tokio 런타임 종료를 블로킹하지 않도록 독립 감시 스레드에서 유한 대기(200ms) 루프로 감시하며,
                                                    // 에이전트 종료 시(shutdown_flag) 루프를 즉시 탈출합니다.
                                                    // 소유 핸들은 정상 종료/취소/오류 등 어떤 경로에서도 스레드 종료 시 Drop되어 안전하게 닫힙니다.
                                                    let tx = tx_self.clone();
                                                    let shutdown_flag = shutdown_requested.clone();
                                                    let _ = std::thread::Builder::new()
                                                        .name("installer-monitor".into())
                                                        .spawn(move || {
                                                            loop {
                                                                if shutdown_flag.load(Ordering::Relaxed) {
                                                                    break;
                                                                }
                                                                match safe_handle.wait_timeout(200) {
                                                                    Ok(crate::updater::ProcessWaitResult::Exited(exit_code)) => {
                                                                        if exit_code != 0 {
                                                                            let _ = tx.try_send(Update::Notice(
                                                                                "설치 중단".into(),
                                                                                "설치 프로그램이 취소되었거나 정상 완료되지 않았습니다.".into(),
                                                                            ));
                                                                        }
                                                                        let _ = tx.try_send(Update::Updater(
                                                                            UpdateStatus::ReadyToInstall {
                                                                                installer_path: installer_path_clone,
                                                                                version: version_clone,
                                                                                install_kind: install_kind_val,
                                                                            },
                                                                        ));
                                                                        break;
                                                                    }
                                                                    Ok(crate::updater::ProcessWaitResult::Timeout) => {}
                                                                    Err(e) => {
                                                                        tracing::warn!(error = %e, "설치 프로그램 대기 중 오류 발생");
                                                                        let _ = tx.try_send(Update::Notice(
                                                                            "설치 오류".into(),
                                                                            format!("설치 프로그램 감시 실패: {e}"),
                                                                        ));
                                                                        let _ = tx.try_send(Update::Updater(
                                                                            UpdateStatus::ReadyToInstall {
                                                                                installer_path: installer_path_clone,
                                                                                version: version_clone,
                                                                                install_kind: install_kind_val,
                                                                            },
                                                                        ));
                                                                        break;
                                                                    }
                                                                }
                                                            }
                                                        });
                                                }
                                                Err(e) => {
                                                    balloon(
                                                        &mut icon,
                                                        "설치 실행 실패",
                                                        &e.to_string(),
                                                    );
                                                    update_status = UpdateStatus::ReadyToInstall {
                                                        installer_path: installer_path_clone,
                                                        version: version_clone,
                                                        install_kind: install_kind_val,
                                                    };
                                                }
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                } else if msg.message == RESET {
                    icon.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
                    let _ = Shell_NotifyIconW(NIM_ADD, &icon);
                } else {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Update::Updater(st)) => {
                    update_status = st;
                }
                Ok(Update::Status(status)) => {
                    wide(&mut icon.szTip, &tooltip(&status));
                    icon.uFlags = NIF_TIP;
                    let _ = Shell_NotifyIconW(NIM_MODIFY, &icon);
                    for drive in status.drives {
                        let old = states.insert(drive.id.clone(), drive.state.clone());
                        if old.as_ref() != Some(&drive.state) {
                            let state = match drive.state {
                                DriveState::Mounted => "마운트 완료".to_owned(),
                                DriveState::Unmounted if old.is_some() => "마운트 해제".to_owned(),
                                DriveState::Failed(e) => format!("작업 실패: {e}"),
                                _ => continue,
                            };
                            balloon(
                                &mut icon,
                                "마운트 상태",
                                &format!("{} ({}): {state}", drive.label, drive.mount_point),
                            );
                        }
                    }
                }
                Ok(Update::Notice(title, body)) => balloon(&mut icon, &title, &body),
                #[cfg(test)]
                Ok(Update::SessionEvent(code)) => {
                    SendMessageW(
                        hwnd,
                        WM_WTSSESSION_CHANGE,
                        Some(WPARAM(code as usize)),
                        None,
                    );
                }
                Ok(Update::Shutdown(done)) => {
                    shutdown_requested.store(true, Ordering::SeqCst);
                    let _ = Shell_NotifyIconW(NIM_DELETE, &icon);
                    let _ = DestroyIcon(app_icon);
                    drop(session_events);
                    let _ = DestroyWindow(hwnd);
                    drop(rx);
                    let _ = done.send(());
                    return Ok(());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        shutdown_requested.store(true, Ordering::SeqCst);
        let _ = Shell_NotifyIconW(NIM_DELETE, &icon);
        let _ = DestroyIcon(app_icon);
        drop(session_events);
        let _ = DestroyWindow(hwnd);
        Ok(())
    }
}

struct SessionNotifications {
    hwnd: HWND,
    registered: bool,
    retry_at: Instant,
}
impl SessionNotifications {
    fn register(&mut self) {
        self.retry_at = Instant::now();
        match unsafe { WTSRegisterSessionNotification(self.hwnd, NOTIFY_FOR_THIS_SESSION) } {
            Ok(()) => self.registered = true,
            Err(e) => tracing::debug!(error = %e, "Windows 세션 알림 등록을 다시 시도합니다"),
        }
    }
}
impl Drop for SessionNotifications {
    fn drop(&mut self) {
        if self.registered {
            unsafe {
                let _ = WTSUnRegisterSessionNotification(self.hwnd);
            }
        }
    }
}

unsafe fn balloon(icon: &mut NOTIFYICONDATAW, title: &str, body: &str) {
    wide(&mut icon.szInfoTitle, title);
    wide(&mut icon.szInfo, body);
    icon.uFlags = NIF_INFO;
    icon.dwInfoFlags = NIIF_INFO;
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, icon);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_tray_starts_and_removes_icon_on_shutdown() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let session = Arc::new(Notify::new());
        let tray = Tray::start(
            rt.handle().clone(),
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
            session.clone(),
        )
        .unwrap();
        tray.0
            .try_send(Update::SessionEvent(WTS_SESSION_UNLOCK))
            .unwrap();
        rt.block_on(async {
            assert!(
                tokio::time::timeout(Duration::from_millis(100), session.notified())
                    .await
                    .is_err()
            );
        });
        tray.0
            .try_send(Update::SessionEvent(WTS_SESSION_LOCK))
            .unwrap();
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), session.notified())
                .await
                .unwrap();
        });
        tray.status(AgentStatus::default());
        assert!(
            tray.shutdown(),
            "트레이 초기화와 종료 응답을 확인해야 합니다"
        );
        assert!(
            tray.0
                .try_send(Update::Status(AgentStatus::default()))
                .is_err()
        );
    }

    #[test]
    fn update_status_transitions_and_menu_rendering() {
        let candidate = crate::updater::UpdateCandidate {
            version: semver::Version::new(0, 1, 1),
            version_str: "0.1.1".into(),
            release_name: "v0.1.1".into(),
            release_url: "https://example.com/release".into(),
            install_kind: crate::updater::InstallKind::Nsis,
            setup_asset: crate::updater::AssetDownloadTarget {
                name: "ExpanDrv_0.1.1_x64-setup.exe".into(),
                size: 1000,
                url: "https://example.com/setup.exe".into(),
            },
            manifest_asset: crate::updater::AssetDownloadTarget {
                name: "ExpanDrv_0.1.1_manifest.json".into(),
                size: 200,
                url: "https://example.com/manifest.json".into(),
            },
        };

        // 1. Idle 상태
        let idle = UpdateStatus::Idle;
        let (label, enabled, id) = menu_item_for_update_status(&idle);
        assert_eq!(label, "업데이트 확인");
        assert!(
            enabled,
            "Idle 상태에서는 업데이트 확인 메뉴가 활성화되어야 합니다"
        );
        assert_eq!(id, 10);

        // 2. Checking 상태 (중복 실행 방지: 메뉴 비활성화)
        let checking = UpdateStatus::Checking;
        let (label, enabled, id) = menu_item_for_update_status(&checking);
        assert_eq!(label, "업데이트 확인 중...");
        assert!(
            !enabled,
            "Checking 상태에서는 중복 실행 방지를 위해 비활성화되어야 합니다"
        );
        assert_eq!(id, 10);

        // 3. Available 상태
        let available = UpdateStatus::Available(candidate.clone());
        let (label, enabled, id) = menu_item_for_update_status(&available);
        assert_eq!(label, "v0.1.1 다운로드 및 설치");
        assert!(
            enabled,
            "Available 상태에서는 다운로드 메뉴가 활성화되어야 합니다"
        );
        assert_eq!(id, 11);

        // 4. Downloading 상태 (중복 다운로드 방지: 메뉴 비활성화)
        let downloading = UpdateStatus::Downloading;
        let (label, enabled, id) = menu_item_for_update_status(&downloading);
        assert_eq!(label, "업데이트 다운로드 중...");
        assert!(
            !enabled,
            "Downloading 상태에서는 중복 실행 방지를 위해 비활성화되어야 합니다"
        );
        assert_eq!(id, 10);

        // 5. ReadyToInstall 상태
        let ready = UpdateStatus::ReadyToInstall {
            installer_path: std::path::PathBuf::from(r"C:\temp\setup.exe"),
            version: "0.1.1".into(),
            install_kind: crate::updater::InstallKind::Nsis,
        };
        let (label, enabled, id) = menu_item_for_update_status(&ready);
        assert_eq!(label, "v0.1.1 지금 설치");
        assert!(
            enabled,
            "ReadyToInstall 상태에서는 설치 실행 메뉴가 활성화되어야 합니다"
        );
        assert_eq!(id, 12);

        // 6. Installing 상태 (단일 실행 보장: 실행 중 중복 클릭 차단)
        let installing = UpdateStatus::Installing {
            version: "0.1.1".into(),
        };
        let (label, enabled, id) = menu_item_for_update_status(&installing);
        assert_eq!(label, "v0.1.1 설치 진행 중...");
        assert!(
            !enabled,
            "Installing 상태에서는 중복 실행 방지를 위해 비활성화되어야 합니다"
        );
        assert_eq!(id, 10);

        // 7. 설치 취소/실패 복구: Installing에서 다시 ReadyToInstall로 복귀 가능 검증
        let mut current_state = installing;
        assert_eq!(
            current_state,
            UpdateStatus::Installing {
                version: "0.1.1".into()
            }
        );
        current_state = ready;
        let (label, enabled, id) = menu_item_for_update_status(&current_state);
        assert_eq!(label, "v0.1.1 지금 설치");
        assert!(enabled);
        assert_eq!(id, 12);
    }

    #[test]
    fn tooltip_summarizes_state() {
        assert_eq!(
            tooltip(&AgentStatus::default()),
            "ExpanDrv | 마운트 0개 | 업로드 대기 0개 | 오류 0개"
        );
    }

    #[test]
    fn tooltip_truncates_without_splitting_surrogates() {
        let mut buf = [0; 4];
        wide(&mut buf, "가😀나");
        assert_eq!(String::from_utf16(&buf[..3]).unwrap(), "가😀");
        assert_eq!(buf[3], 0);
    }
}
