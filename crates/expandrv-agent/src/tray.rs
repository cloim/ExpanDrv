//! 에이전트 수명과 함께 동작하는 Windows 알림 영역.
use expandrv_core::config::{NotificationConfig, NotificationKind};
use expandrv_ipc::{AgentStatus, DriveState};
use expandrv_vfs::{TransferEvent, Vfs};
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, mpsc};
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
/// 업데이트 상태가 바뀌면 열려 있는 트레이 메뉴의 업데이트 항목을 고칩니다.
const REFRESH_MENU: u32 = WM_APP + 4;

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// 에이전트 시작 뒤 첫 업데이트 확인까지 기다리는 시간
const FIRST_UPDATE_CHECK: Duration = Duration::from_secs(10);
/// 주기적 업데이트 확인 간격
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// 트레이 메뉴를 열 때 다시 확인하기까지의 최소 간격. GitHub API 호출 한도를 아낍니다.
const MENU_CHECK_MIN_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct SessionNotifiers {
    pub lock: Arc<Notify>,
    pub unlock: Arc<Notify>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateStatus {
    /// 아직 확인하지 않았습니다.
    Idle,
    Checking,
    /// 현재 버전이 최신입니다.
    UpToDate,
    /// 개발 빌드처럼 설치 형식을 알 수 없어 자동 업데이트를 지원하지 않습니다.
    Unsupported,
    /// 확인에 실패했습니다. 메뉴를 누르면 다시 확인합니다.
    Failed,
    Available(crate::updater::UpdateCandidate),
    Downloading {
        version: String,
    },
    ReadyToInstall {
        installer_path: std::path::PathBuf,
        version: String,
        install_kind: crate::updater::InstallKind,
    },
    Installing {
        version: String,
    },
}

impl UpdateStatus {
    /// 다운로드부터 설치까지 진행 중이면 확인 결과로 덮지 않습니다.
    fn in_progress(&self) -> bool {
        matches!(
            self,
            Self::Downloading { .. } | Self::ReadyToInstall { .. } | Self::Installing { .. }
        )
    }
}

/// 업데이트 상태에 따른 트레이 메뉴 라벨, 활성화 여부(클릭 가능 여부) 및 메뉴 ID를 반환합니다.
pub fn menu_item_for_update_status(status: &UpdateStatus) -> (String, bool, u32) {
    match status {
        UpdateStatus::Idle => ("업데이트 확인".into(), true, 10),
        UpdateStatus::Checking => ("업데이트 확인 중...".into(), false, 10),
        UpdateStatus::UpToDate => (format!("최신 버전 (v{CURRENT_VERSION})"), false, 10),
        UpdateStatus::Unsupported => (format!("업데이트 미지원 (v{CURRENT_VERSION})"), false, 10),
        UpdateStatus::Failed => (format!("업데이트 확인 실패 (v{CURRENT_VERSION})"), true, 10),
        UpdateStatus::Available(c) => (format!("업데이트 하기 (v{})", c.version_str), true, 11),
        UpdateStatus::Downloading { version } => {
            (format!("업데이트 다운로드 중 (v{version})"), false, 11)
        }
        UpdateStatus::ReadyToInstall { version, .. } => {
            (format!("업데이트 하기 (v{version})"), true, 12)
        }
        UpdateStatus::Installing { version } => {
            (format!("업데이트 설치 중 (v{version})"), false, 12)
        }
    }
}

/// 트레이 스레드, 창 프로시저와 업데이트 작업이 함께 쓰는 상태
struct Shared {
    notifiers: Arc<SessionNotifiers>,
    update: Mutex<UpdateStatus>,
    checking: AtomicBool,
    last_check: Mutex<Option<Instant>>,
    /// 열려 있는 트레이 메뉴 핸들(없으면 0)
    menu: AtomicIsize,
    /// 트레이 창 핸들(종료 뒤 0)
    hwnd: AtomicIsize,
}

impl Shared {
    fn update_status(&self) -> UpdateStatus {
        self.update
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn set_update(&self, status: UpdateStatus) {
        *self.update.lock().unwrap_or_else(|e| e.into_inner()) = status;
        self.refresh_menu();
    }

    /// 열려 있는 메뉴가 있으면 창 프로시저가 업데이트 항목을 고치게 합니다.
    fn refresh_menu(&self) {
        let hwnd = self.hwnd.load(Ordering::SeqCst);
        if hwnd != 0 {
            unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd as *mut _)),
                    REFRESH_MENU,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
}

/// 대분류별 알림 표시 여부. 설정을 다시 읽으면 즉시 바뀌고, 대기 중인 알림에도 적용합니다.
pub struct NotificationGate(AtomicU8);

impl NotificationGate {
    pub fn new(config: NotificationConfig) -> Self {
        let gate = Self(AtomicU8::new(0));
        gate.set(config);
        gate
    }

    pub fn set(&self, config: NotificationConfig) {
        let bits = [
            NotificationKind::MountStatus,
            NotificationKind::FileTransfers,
            NotificationKind::LockRecovery,
            NotificationKind::AutoUpdater,
        ]
        .into_iter()
        .filter(|&kind| config.allows(kind))
        .fold(0, |bits, kind| bits | Self::bit(kind));
        self.0.store(bits, Ordering::Relaxed);
    }

    pub fn allows(&self, kind: NotificationKind) -> bool {
        self.0.load(Ordering::Relaxed) & Self::bit(kind) != 0
    }

    fn bit(kind: NotificationKind) -> u8 {
        match kind {
            NotificationKind::MountStatus => 1,
            NotificationKind::FileTransfers => 2,
            NotificationKind::LockRecovery => 4,
            NotificationKind::AutoUpdater => 8,
        }
    }
}

enum Update {
    Status(AgentStatus),
    Notice(NotificationKind, String, String),
    Shutdown(mpsc::Sender<()>),
    /// 다운로드를 마친 설치 패키지를 트레이 스레드에서 실행합니다.
    Install(UpdateStatus),
    #[cfg(test)]
    SessionEvent(u32),
}

#[derive(Clone)]
pub struct Tray {
    tx: mpsc::SyncSender<Update>,
    gate: Arc<NotificationGate>,
}

impl Tray {
    pub fn start(
        rt: tokio::runtime::Handle,
        stop: Arc<Notify>,
        lock: Arc<Notify>,
        session_lock: Arc<Notify>,
        session_unlock: Arc<Notify>,
        notifications: NotificationConfig,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(128);
        let tx_clone = tx.clone();
        let gate = Arc::new(NotificationGate::new(notifications));
        let thread_gate = gate.clone();
        let notifiers = Arc::new(SessionNotifiers {
            lock: session_lock,
            unlock: session_unlock,
        });
        std::thread::Builder::new()
            .name("expandrv-tray".into())
            .spawn(move || {
                if let Err(e) = unsafe { run(rx, tx_clone, rt, stop, lock, notifiers, thread_gate) }
                {
                    tracing::warn!(error = %e, "트레이를 시작하지 못했습니다");
                }
            })?;
        Ok(Self { tx, gate })
    }

    pub fn shutdown(&self) -> bool {
        let (tx, rx) = mpsc::channel();
        self.tx.try_send(Update::Shutdown(tx)).is_ok()
            && rx.recv_timeout(Duration::from_secs(1)).is_ok()
    }

    pub fn status(&self, status: AgentStatus) {
        let _ = self.tx.try_send(Update::Status(status));
    }

    /// 설정을 다시 읽은 뒤 알림 표시 여부를 바꿉니다.
    pub fn set_notifications(&self, config: NotificationConfig) {
        self.gate.set(config);
    }

    /// 끈 대분류의 알림은 보내지 않습니다.
    pub fn notice(&self, kind: NotificationKind, title: &str, body: &str) {
        if self.gate.allows(kind) {
            let _ = self
                .tx
                .try_send(Update::Notice(kind, title.into(), body.into()));
        }
    }

    /// 전송 이벤트는 알림을 꺼도 비워서 쌓이지 않게 합니다.
    pub fn transfers(&self, id: &str, vfs: &Vfs) {
        for event in vfs.take_transfer_events() {
            let (title, path) = match event {
                TransferEvent::Uploaded(p) => ("업로드 완료", p),
                TransferEvent::Conflict(p) => ("충돌 사본 생성", p),
            };
            self.notice(
                NotificationKind::FileTransfers,
                title,
                &format!("{id}: {path}"),
            );
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
            let code = wp.0 as u32;
            let shared = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Shared;
            if let Some(shared) = shared.as_ref() {
                if code == WTS_SESSION_LOCK {
                    shared.notifiers.lock.notify_one();
                } else if code == WTS_SESSION_UNLOCK {
                    shared.notifiers.unlock.notify_one();
                }
            }
            return LRESULT(0);
        }
        if msg == REFRESH_MENU {
            // 메뉴가 열려 있는 동안에도 메뉴 모달 루프가 이 메시지를 전달합니다.
            let shared = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Shared;
            if let Some(shared) = shared.as_ref() {
                let menu = shared.menu.load(Ordering::SeqCst);
                if menu != 0 {
                    put_update_item(HMENU(menu as *mut _), &shared.update_status(), false);
                    if let Ok(popup) = FindWindowW(w!("#32768"), None) {
                        let _ =
                            windows::Win32::Graphics::Gdi::InvalidateRect(Some(popup), None, true);
                    }
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

/// 업데이트 항목은 메뉴의 첫 번째 자리에 둡니다. `append`가 아니면 그 자리를 고칩니다.
unsafe fn put_update_item(menu: HMENU, status: &UpdateStatus, append: bool) {
    let (label, enabled, id) = menu_item_for_update_status(status);
    let text: Vec<u16> = label.encode_utf16().chain(std::iter::once(0)).collect();
    let flags = if enabled {
        MF_STRING
    } else {
        MF_STRING | MF_GRAYED
    };
    let text = windows::core::PCWSTR(text.as_ptr());
    unsafe {
        let _ = if append {
            AppendMenuW(menu, flags, id as usize, text)
        } else {
            ModifyMenuW(menu, 0, flags | MF_BYPOSITION, id as usize, text)
        };
    }
}

async fn check_latest(
    install_kind: crate::updater::InstallKind,
) -> Result<Option<crate::updater::UpdateCandidate>, String> {
    let client = crate::updater::build_http_client().map_err(|e| e.to_string())?;
    let current = crate::updater::parse_version(CURRENT_VERSION)
        .ok_or_else(|| "현재 버전 형식을 확인할 수 없습니다".to_owned())?;
    let releases = crate::updater::fetch_releases(
        &client,
        crate::updater::DEFAULT_REPO_OWNER,
        crate::updater::DEFAULT_REPO_NAME,
    )
    .await
    .map_err(|e| e.to_string())?;
    crate::updater::find_latest_update(&releases, &current, install_kind).map_err(|e| e.to_string())
}

/// 업데이트 확인을 백그라운드로 시작합니다.
/// 이미 확인 중이거나, 다운로드부터 설치까지 진행 중이거나, `min_interval` 안에 확인했다면 건너뜁니다.
fn start_update_check(
    shared: &Arc<Shared>,
    rt: &tokio::runtime::Handle,
    tx: &mpsc::SyncSender<Update>,
    min_interval: Duration,
) {
    let status = shared.update_status();
    if status.in_progress() {
        return;
    }
    {
        let mut last = shared.last_check.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|at| at.elapsed() < min_interval)
            || shared.checking.swap(true, Ordering::SeqCst)
        {
            return;
        }
        *last = Some(Instant::now());
    }
    let install_kind = crate::updater::detect_current_install_kind();
    if install_kind == crate::updater::InstallKind::Unknown {
        shared.set_update(UpdateStatus::Unsupported);
        shared.checking.store(false, Ordering::SeqCst);
        return;
    }
    // 이미 알고 있는 결과(최신 버전, 새 버전)는 확인하는 동안 그대로 보여 줍니다.
    if matches!(status, UpdateStatus::Idle | UpdateStatus::Failed) {
        shared.set_update(UpdateStatus::Checking);
    }
    let (shared, tx) = (shared.clone(), tx.clone());
    rt.spawn(async move {
        let result = check_latest(install_kind).await;
        let found = {
            let mut st = shared.update.lock().unwrap_or_else(|e| e.into_inner());
            match result {
                // 확인하는 동안 다운로드를 시작했다면 그 진행 상태를 지킵니다.
                _ if st.in_progress() => None,
                Ok(Some(candidate)) => {
                    let known = matches!(&*st, UpdateStatus::Available(c) if c.version == candidate.version);
                    let version = candidate.version_str.clone();
                    *st = UpdateStatus::Available(candidate);
                    (!known).then_some(version)
                }
                Ok(None) => {
                    *st = UpdateStatus::UpToDate;
                    None
                }
                Err(e) => {
                    tracing::warn!(error = %e, "업데이트 확인 실패");
                    if matches!(*st, UpdateStatus::Idle | UpdateStatus::Checking) {
                        *st = UpdateStatus::Failed;
                    }
                    None
                }
            }
        };
        shared.checking.store(false, Ordering::SeqCst);
        shared.refresh_menu();
        if let Some(version) = found {
            let _ = tx.try_send(Update::Notice(
                NotificationKind::AutoUpdater,
                "업데이트 가능".into(),
                format!("ExpanDrv v{version}을 설치할 수 있습니다. 트레이 메뉴의 `업데이트 하기`를 누르면 바로 설치합니다."),
            ));
        }
    });
}

async fn download_update(
    candidate: &crate::updater::UpdateCandidate,
) -> Result<std::path::PathBuf, String> {
    let client = crate::updater::build_http_client().map_err(|e| e.to_string())?;
    let manifest = crate::updater::fetch_manifest(&client, &candidate.manifest_asset.url)
        .await
        .map_err(|e| format!("매니페스트 확인 실패: {e}"))?;
    let artifact = crate::updater::verify_manifest(&manifest, candidate)
        .map_err(|e| format!("매니페스트 검증 실패: {e}"))?
        .clone();
    crate::updater::download_and_verify_setup(&client, candidate, &artifact)
        .await
        .map_err(|e| format!("다운로드 검증 실패: {e}"))
}

/// 설치 패키지를 받아 검증한 뒤 트레이 스레드에 설치 실행을 요청합니다.
fn start_download(
    shared: &Arc<Shared>,
    rt: &tokio::runtime::Handle,
    tx: &mpsc::SyncSender<Update>,
    candidate: crate::updater::UpdateCandidate,
) {
    shared.set_update(UpdateStatus::Downloading {
        version: candidate.version_str.clone(),
    });
    let (shared, tx) = (shared.clone(), tx.clone());
    rt.spawn(async move {
        match download_update(&candidate).await {
            Ok(installer_path) => {
                let ready = UpdateStatus::ReadyToInstall {
                    installer_path,
                    version: candidate.version_str,
                    install_kind: candidate.install_kind,
                };
                shared.set_update(ready.clone());
                // 설치 프로그램은 트레이 스레드에서 실행합니다. 보내지 못하면 메뉴에서 다시 누를 수 있습니다.
                let _ = tx.try_send(Update::Install(ready));
            }
            Err(e) => {
                tracing::warn!(error = %e, "업데이트 다운로드 실패");
                shared.set_update(UpdateStatus::Available(candidate));
                let _ = tx.try_send(Update::Notice(
                    NotificationKind::AutoUpdater,
                    "업데이트 실패".into(),
                    e,
                ));
            }
        }
    });
}

/// 설치 프로그램을 실행하고 끝날 때까지 감시합니다.
/// 인스톨러의 사전 실행 훅이 스스로 `expandrv.exe stop`을 호출하여 미완료 업로드 대기와 드라이브 해제를
/// 검증하므로, 에이전트가 먼저 종료해 경합하거나 관측을 방해하지 않습니다.
unsafe fn start_install(
    shared: &Arc<Shared>,
    tx: &mpsc::SyncSender<Update>,
    shutdown_requested: &Arc<AtomicBool>,
    gate: &NotificationGate,
    icon: &mut NOTIFYICONDATAW,
    ready: UpdateStatus,
) {
    let UpdateStatus::ReadyToInstall {
        installer_path,
        version,
        install_kind,
    } = &ready
    else {
        return;
    };
    // 중복 실행을 막기 위해 먼저 설치 중으로 바꿉니다.
    shared.set_update(UpdateStatus::Installing {
        version: version.clone(),
    });
    match crate::updater::launch_installer(installer_path, *install_kind) {
        Ok(handle) => {
            unsafe {
                balloon(
                    gate,
                    NotificationKind::AutoUpdater,
                    icon,
                    "업데이트 설치",
                    &format!("v{version} 설치 프로그램을 시작했습니다."),
                );
            }
            // 사용자가 마법사에서 취소하거나 설치가 중단되어 에이전트가 살아 있으면 다시 설치할 수 있게 되돌립니다.
            // Tokio 런타임 종료를 막지 않도록 별도 스레드에서 200ms씩 기다리고, 에이전트 종료 시 바로 빠져나옵니다.
            // 소유 핸들은 어떤 경로로 끝나도 스레드 종료 시 Drop되어 닫힙니다.
            let (shared, tx, shutdown) = (shared.clone(), tx.clone(), shutdown_requested.clone());
            let _ = std::thread::Builder::new()
                .name("installer-monitor".into())
                .spawn(move || {
                    while !shutdown.load(Ordering::Relaxed) {
                        match handle.wait_timeout(200) {
                            Ok(crate::updater::ProcessWaitResult::Timeout) => continue,
                            Ok(crate::updater::ProcessWaitResult::Exited(code)) => {
                                if code != 0 {
                                    let _ = tx.try_send(Update::Notice(
                                        NotificationKind::AutoUpdater,
                                        "설치 중단".into(),
                                        "설치 프로그램이 취소되었거나 정상 완료되지 않았습니다."
                                            .into(),
                                    ));
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "설치 프로그램 대기 중 오류 발생");
                                let _ = tx.try_send(Update::Notice(
                                    NotificationKind::AutoUpdater,
                                    "설치 오류".into(),
                                    format!("설치 프로그램 감시 실패: {e}"),
                                ));
                            }
                        }
                        shared.set_update(ready);
                        break;
                    }
                });
        }
        Err(e) => {
            unsafe {
                balloon(
                    gate,
                    NotificationKind::AutoUpdater,
                    icon,
                    "설치 실행 실패",
                    &e.to_string(),
                );
            }
            shared.set_update(ready.clone());
        }
    }
}

unsafe fn run(
    rx: mpsc::Receiver<Update>,
    tx_self: mpsc::SyncSender<Update>,
    rt: tokio::runtime::Handle,
    stop: Arc<Notify>,
    lock: Arc<Notify>,
    notifiers: Arc<SessionNotifiers>,
    gate: Arc<NotificationGate>,
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
        let shared = Arc::new(Shared {
            notifiers,
            update: Mutex::new(UpdateStatus::Idle),
            checking: AtomicBool::new(false),
            last_check: Mutex::new(None),
            menu: AtomicIsize::new(0),
            hwnd: AtomicIsize::new(hwnd.0 as isize),
        });
        // Shared의 Arc는 창 파괴까지 살아 있으므로 WindowProc의 포인터도 유효합니다.
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Arc::as_ptr(&shared) as isize);
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
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let mut next_update_check = Instant::now() + FIRST_UPDATE_CHECK;
        let cleanup = |icon: &NOTIFYICONDATAW, session_events: SessionNotifications| {
            shutdown_requested.store(true, Ordering::SeqCst);
            shared.hwnd.store(0, Ordering::SeqCst);
            let _ = Shell_NotifyIconW(NIM_DELETE, icon);
            let _ = DestroyIcon(app_icon);
            drop(session_events);
            let _ = DestroyWindow(hwnd);
        };
        loop {
            if !session_events.registered
                && session_events.retry_at.elapsed() >= Duration::from_secs(5)
            {
                session_events.register();
            }
            if Instant::now() >= next_update_check {
                next_update_check = Instant::now() + UPDATE_CHECK_INTERVAL;
                start_update_check(&shared, &rt, &tx_self, Duration::ZERO);
            }
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == ACTION {
                    match msg.lParam.0 as u32 {
                        WM_LBUTTONDBLCLK => open_gui(),
                        WM_RBUTTONUP | WM_CONTEXTMENU => {
                            // 메뉴를 여는 동시에 확인을 시작하고, 결과는 열린 메뉴에 바로 반영합니다.
                            start_update_check(&shared, &rt, &tx_self, MENU_CHECK_MIN_INTERVAL);
                            let menu = CreatePopupMenu()?;
                            put_update_item(menu, &shared.update_status(), true);
                            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
                            let _ = AppendMenuW(menu, MF_STRING, 1, w!("열기"));
                            let _ = AppendMenuW(menu, MF_STRING, 3, w!("잠금"));
                            let _ = AppendMenuW(menu, MF_STRING, 2, w!("종료"));
                            shared.menu.store(menu.0 as isize, Ordering::SeqCst);

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
                            shared.menu.store(0, Ordering::SeqCst);
                            let _ = DestroyMenu(menu);
                            match (choice, shared.update_status()) {
                                (1, _) => open_gui(),
                                (2, _) => stop.notify_one(),
                                (3, _) => lock.notify_one(),
                                (10, UpdateStatus::Idle | UpdateStatus::Failed) => {
                                    start_update_check(&shared, &rt, &tx_self, Duration::ZERO)
                                }
                                (11, UpdateStatus::Available(candidate)) => {
                                    balloon(
                                        &gate,
                                        NotificationKind::AutoUpdater,
                                        &mut icon,
                                        "업데이트 다운로드",
                                        &format!(
                                            "ExpanDrv v{} 설치 패키지를 받은 뒤 바로 설치합니다.",
                                            candidate.version_str
                                        ),
                                    );
                                    start_download(&shared, &rt, &tx_self, candidate);
                                }
                                (12, ready @ UpdateStatus::ReadyToInstall { .. }) => start_install(
                                    &shared,
                                    &tx_self,
                                    &shutdown_requested,
                                    &gate,
                                    &mut icon,
                                    ready,
                                ),
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
                Ok(Update::Install(ready)) => {
                    // 다운로드를 마친 뒤 그 사이 다른 설치가 시작되지 않았을 때만 실행합니다.
                    if shared.update_status() == ready {
                        start_install(
                            &shared,
                            &tx_self,
                            &shutdown_requested,
                            &gate,
                            &mut icon,
                            ready,
                        );
                    }
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
                                &gate,
                                NotificationKind::MountStatus,
                                &mut icon,
                                "마운트 상태",
                                &format!("{} ({}): {state}", drive.label, drive.mount_point),
                            );
                        }
                    }
                }
                Ok(Update::Notice(kind, title, body)) => {
                    balloon(&gate, kind, &mut icon, &title, &body)
                }
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
                    cleanup(&icon, session_events);
                    drop(rx);
                    let _ = done.send(());
                    return Ok(());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        cleanup(&icon, session_events);
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

/// 끈 대분류의 알림은 표시하지 않습니다.
unsafe fn balloon(
    gate: &NotificationGate,
    kind: NotificationKind,
    icon: &mut NOTIFYICONDATAW,
    title: &str,
    body: &str,
) {
    if !gate.allows(kind) {
        return;
    }
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
        let session_lock = Arc::new(Notify::new());
        let session_unlock = Arc::new(Notify::new());
        let tray = Tray::start(
            rt.handle().clone(),
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
            session_lock.clone(),
            session_unlock.clone(),
            NotificationConfig::default(),
        )
        .unwrap();
        tray.tx
            .try_send(Update::SessionEvent(WTS_SESSION_UNLOCK))
            .unwrap();
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), session_unlock.notified())
                .await
                .unwrap();
        });
        tray.tx
            .try_send(Update::SessionEvent(WTS_SESSION_LOCK))
            .unwrap();
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), session_lock.notified())
                .await
                .unwrap();
        });
        tray.status(AgentStatus::default());
        assert!(
            tray.shutdown(),
            "트레이 초기화와 종료 응답을 확인해야 합니다"
        );
        assert!(
            tray.tx
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

        let cur = env!("CARGO_PKG_VERSION");
        let cases = [
            (UpdateStatus::Idle, "업데이트 확인".to_owned(), true, 10),
            (
                UpdateStatus::Checking,
                "업데이트 확인 중...".to_owned(),
                false,
                10,
            ),
            (
                UpdateStatus::UpToDate,
                format!("최신 버전 (v{cur})"),
                false,
                10,
            ),
            (
                UpdateStatus::Unsupported,
                format!("업데이트 미지원 (v{cur})"),
                false,
                10,
            ),
            (
                UpdateStatus::Failed,
                format!("업데이트 확인 실패 (v{cur})"),
                true,
                10,
            ),
            (
                UpdateStatus::Available(candidate.clone()),
                "업데이트 하기 (v0.1.1)".to_owned(),
                true,
                11,
            ),
            (
                UpdateStatus::Downloading {
                    version: "0.1.1".into(),
                },
                "업데이트 다운로드 중 (v0.1.1)".to_owned(),
                false,
                11,
            ),
            (
                UpdateStatus::ReadyToInstall {
                    installer_path: std::path::PathBuf::from(r"C:\temp\setup.exe"),
                    version: "0.1.1".into(),
                    install_kind: crate::updater::InstallKind::Nsis,
                },
                "업데이트 하기 (v0.1.1)".to_owned(),
                true,
                12,
            ),
            (
                UpdateStatus::Installing {
                    version: "0.1.1".into(),
                },
                "업데이트 설치 중 (v0.1.1)".to_owned(),
                false,
                12,
            ),
        ];
        for (status, label, enabled, id) in cases {
            assert_eq!(
                menu_item_for_update_status(&status),
                (label, enabled, id),
                "{status:?}"
            );
        }
        // 다운로드부터 설치까지는 확인 결과로 덮지 않습니다.
        assert!(!UpdateStatus::Available(candidate).in_progress());
        assert!(
            UpdateStatus::Downloading {
                version: "0.1.1".into()
            }
            .in_progress()
        );
    }

    #[test]
    fn notification_gate_follows_config() {
        let mut config = NotificationConfig::default();
        let gate = NotificationGate::new(config);
        for kind in [
            NotificationKind::MountStatus,
            NotificationKind::FileTransfers,
            NotificationKind::LockRecovery,
            NotificationKind::AutoUpdater,
        ] {
            assert!(gate.allows(kind));
        }
        config.mount_status = false;
        config.auto_updater = false;
        gate.set(config);
        assert!(!gate.allows(NotificationKind::MountStatus));
        assert!(gate.allows(NotificationKind::FileTransfers));
        assert!(gate.allows(NotificationKind::LockRecovery));
        assert!(!gate.allows(NotificationKind::AutoUpdater));
    }

    #[test]
    fn disabled_category_notice_is_not_queued_and_reload_applies() {
        let (tx, rx) = mpsc::sync_channel(8);
        let config = NotificationConfig {
            lock_recovery: false,
            ..NotificationConfig::default()
        };
        let tray = Tray {
            tx,
            gate: Arc::new(NotificationGate::new(config)),
        };
        tray.notice(NotificationKind::LockRecovery, "자동 잠금", "막혀야 합니다");
        assert!(
            rx.try_recv().is_err(),
            "끈 대분류 알림은 보내지 않아야 합니다"
        );
        tray.notice(NotificationKind::FileTransfers, "업로드 완료", "a: /x");
        assert!(matches!(
            rx.try_recv(),
            Ok(Update::Notice(NotificationKind::FileTransfers, _, _))
        ));

        // 설정을 다시 읽으면 같은 트레이에 즉시 반영됩니다.
        tray.set_notifications(NotificationConfig {
            file_transfers: false,
            ..NotificationConfig::default()
        });
        tray.notice(NotificationKind::FileTransfers, "업로드 완료", "a: /x");
        assert!(rx.try_recv().is_err());
        tray.notice(NotificationKind::LockRecovery, "자동 잠금", "보여야 합니다");
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn update_item_is_first_and_replaced_in_place() {
        let label = |menu: HMENU, pos: u32| unsafe {
            let mut buf = [0u16; 64];
            let n = GetMenuStringW(menu, pos, Some(&mut buf), MF_BYPOSITION);
            String::from_utf16_lossy(&buf[..n as usize])
        };
        unsafe {
            let menu = CreatePopupMenu().unwrap();
            put_update_item(menu, &UpdateStatus::Checking, true);
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            let _ = AppendMenuW(menu, MF_STRING, 1, w!("열기"));
            put_update_item(
                menu,
                &UpdateStatus::Downloading {
                    version: "9.9.9".into(),
                },
                false,
            );
            assert_eq!(GetMenuItemCount(Some(menu)), 3);
            assert_eq!(label(menu, 0), "업데이트 다운로드 중 (v9.9.9)");
            assert_eq!(GetMenuItemID(menu, 0), 11);
            assert_eq!(label(menu, 2), "열기");
            let _ = DestroyMenu(menu);
        }
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
