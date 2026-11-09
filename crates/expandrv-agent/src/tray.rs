//! 에이전트 수명과 함께 동작하는 Windows 알림 영역.
use expandrv_ipc::{AgentStatus, DriveState};
use expandrv_vfs::{TransferEvent, Vfs};
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
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

enum Update {
    Status(AgentStatus),
    Notice(String, String),
    Shutdown(mpsc::Sender<()>),
    #[cfg(test)]
    SessionEvent(u32),
}

#[derive(Clone)]
pub struct Tray(mpsc::SyncSender<Update>);

impl Tray {
    pub fn start(
        stop: Arc<Notify>,
        lock: Arc<Notify>,
        session: Arc<Notify>,
    ) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(128);
        std::thread::Builder::new()
            .name("expandrv-tray".into())
            .spawn(move || {
                if let Err(e) = unsafe { run(rx, stop, lock, session) } {
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
        let session = Arc::new(Notify::new());
        let tray = Tray::start(
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
            session.clone(),
        )
        .unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
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
