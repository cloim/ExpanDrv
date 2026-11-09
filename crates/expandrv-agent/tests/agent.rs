//! `expandrv` 바이너리로 상주 에이전트의 mount, list, unmount, stop 흐름을 확인합니다.
//! WinFsp 런타임이 없으면 건너뜁니다.

use expandrv_ipc::IpcClient;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

const CMD_TIMEOUT: Duration = Duration::from_secs(120);

/// 마운트하면 드라이브별 캐시 키가 자격 증명 관리자에 생깁니다.
/// 테스트 전에 없던 키만 끝날 때 지워 사용자의 기존 키는 건드리지 않습니다.
struct CacheKeys(Vec<String>);

struct PinGuard(expandrv_secrets::lock::PinStore);
impl Drop for PinGuard {
    fn drop(&mut self) {
        let _ = self.0.remove("123456");
    }
}

impl CacheKeys {
    fn new(ids: &[&str]) -> Self {
        let store = expandrv_secrets::CredentialStore;
        Self(
            ids.iter()
                .map(|id| expandrv_secrets::target_for(id, "cache_key"))
                .filter(|t| store.get(t).is_err())
                .collect(),
        )
    }
}

impl Drop for CacheKeys {
    fn drop(&mut self) {
        for t in &self.0 {
            let _ = expandrv_secrets::CredentialStore.delete(t);
        }
    }
}

/// 마운트 테스트끼리 같은 드라이브 문자를 고르지 않도록 순서대로 실행합니다.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Z부터 내려가며 쓰지 않는 드라이브 문자를 고릅니다.
fn free_drives(n: usize) -> Vec<String> {
    ('H'..='Z')
        .rev()
        .map(|c| format!("{c}:"))
        .filter(|d| !Path::new(&format!("{d}\\")).exists())
        .take(n)
        .collect()
}

struct Env {
    dir: tempfile::TempDir,
    pipe: String,
}

struct Run {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Env {
    /// 테스트마다 다른 파이프 이름을 써서 병렬 실행 때 서로의 에이전트에 붙지 않게 합니다.
    fn new(tag: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("localappdata")).unwrap();
        Self {
            pipe: format!(r"\\.\pipe\expandrv-itest-{tag}-{}", std::process::id()),
            dir,
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_expandrv"));
        c.arg("--config")
            .arg(self.path("config.toml"))
            .args(args)
            .env("EXPANDRV_PIPE", &self.pipe)
            .env("LOCALAPPDATA", self.path("localappdata"))
            .env_remove("EXPANDRV_LOG")
            .stdin(Stdio::null());
        c
    }

    /// 출력을 파이프로 받습니다. 에이전트가 파이프를 물려받으면 여기서 시간 초과가 납니다.
    fn run(&self, args: &[&str]) -> Run {
        let child = self
            .command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        let out = match rx.recv_timeout(CMD_TIMEOUT) {
            Ok(out) => out.unwrap(),
            Err(_) => {
                kill(pid);
                panic!("시간 초과: expandrv {args:?}");
            }
        };
        let r = Run {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        };
        eprintln!(
            "$ expandrv {args:?} -> {}\n{}{}",
            out.status, r.stdout, r.stderr
        );
        r
    }

    /// 표준 입력 한 줄을 넘겨 실행합니다(비밀 값 입력 확인용).
    fn run_with_input(&self, args: &[&str], input: &str) -> Run {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{input}\n").as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        Run {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn agent_pid(&self) -> Option<u32> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(IpcClient::connect_to(&self.pipe, Duration::ZERO))
            .ok()
            .map(|c| c.server_pid())
    }
}

fn kill(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn wait_exit(pid: u32, timeout: Duration) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return true;
        };
        let r = WaitForSingleObject(h, timeout.as_millis() as u32);
        let _ = CloseHandle(h);
        r == WAIT_OBJECT_0
    }
}

/// 테스트가 중간에 실패해도 에이전트와 마운트를 남기지 않습니다.
struct AgentGuard<'a> {
    env: &'a Env,
    pid: u32,
}

impl Drop for AgentGuard<'_> {
    fn drop(&mut self) {
        if !wait_exit(self.pid, Duration::ZERO) {
            let _ = self.env.command(&["stop"]).output();
            if !wait_exit(self.pid, Duration::from_secs(30)) {
                kill(self.pid);
            }
        }
    }
}

fn line_of<'a>(out: &'a str, id: &str) -> &'a str {
    out.lines()
        .find(|l| l.starts_with(&format!("{id} ")))
        .unwrap_or_else(|| panic!("{id} 행이 없습니다:\n{out}"))
}

/// 테스트 에이전트의 창에만 알림을 보냅니다. Windows 화면 잠금은 실행하지 않습니다.
fn send_session_event(pid: u32, event: u32) {
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowThreadProcessId, PostMessageW, WM_WTSSESSION_CHANGE,
    };
    unsafe extern "system" fn find(hwnd: HWND, data: LPARAM) -> windows::core::BOOL {
        unsafe {
            let target = &mut *(data.0 as *mut (u32, HWND));
            let mut owner = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut owner));
            if owner == target.0 {
                let mut class = [0; 64];
                let n = GetClassNameW(hwnd, &mut class);
                if String::from_utf16_lossy(&class[..n as usize]) == "ExpanDrvTray" {
                    target.1 = hwnd;
                    return false.into();
                }
            }
            true.into()
        }
    }
    let mut target = (pid, HWND::default());
    for _ in 0..100 {
        unsafe {
            let _ = EnumWindows(
                Some(find),
                LPARAM((&mut target as *mut (u32, HWND)) as isize),
            );
        }
        if !target.1.is_invalid() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!target.1.is_invalid(), "테스트 에이전트 창이 필요합니다");
    unsafe {
        PostMessageW(
            Some(target.1),
            WM_WTSSESSION_CHANGE,
            WPARAM(event as usize),
            LPARAM(0),
        )
        .unwrap();
    }
}

#[test]
fn session_lock_respects_pin_policy_and_preserves_pending_upload() {
    use expandrv_core::config::Config;
    use expandrv_ipc::{Pin, Request, Response};
    use windows::Win32::UI::WindowsAndMessaging::{WTS_SESSION_LOCK, WTS_SESSION_UNLOCK};
    if expandrv_winfsp::runtime::runtime_dll().is_none() {
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let env = Env::new("session-lock");
    let _keys = CacheKeys::new(&["session-lock-test"]);
    let _pin = PinGuard(expandrv_secrets::lock::PinStore::new(&env.path("config.toml")).unwrap());
    let root = env.path("remote");
    std::fs::create_dir(&root).unwrap();
    let letter = free_drives(1).remove(0);
    let drive = PathBuf::from(format!("{letter}\\"));
    assert!(
        env.run(&[
            "add",
            "session-lock-test",
            &letter,
            "local",
            root.to_str().unwrap()
        ])
        .ok
    );
    assert!(env.run(&["mount", "session-lock-test"]).ok);
    let pid = env.agent_pid().unwrap();
    let _guard = AgentGuard { env: &env, pid };
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut client = IpcClient::connect_to(&env.pipe, Duration::from_secs(2))
            .await
            .unwrap();
        client.hello().await.unwrap();
        send_session_event(pid, WTS_SESSION_LOCK);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let Response::Status(status) = client.call(&Request::Status).await.unwrap() else {
            panic!("status")
        };
        assert!(!status.locked, "PIN이 없으면 자동 잠금을 시작하지 않습니다");
        assert_eq!(
            client
                .call(&Request::SetPin {
                    current: Pin(String::new()),
                    new: Pin("123456".into())
                })
                .await
                .unwrap(),
            Response::Ok
        );
        let path = env.path("config.toml");
        let mut config = Config::load(&path).unwrap();
        config.security.lock_on_session_lock = false;
        config.save(&path).unwrap();
        assert_eq!(client.call(&Request::Reload).await.unwrap(), Response::Ok);
        send_session_event(pid, WTS_SESSION_LOCK);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let Response::Status(status) = client.call(&Request::Status).await.unwrap() else {
            panic!("status")
        };
        assert!(!status.locked);
        config.security.lock_on_session_lock = true;
        config.save(&path).unwrap();
        assert_eq!(client.call(&Request::Reload).await.unwrap(), Response::Ok);
        std::fs::write(drive.join("pending.txt"), b"session-safe").unwrap();
        send_session_event(pid, WTS_SESSION_LOCK);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let Response::Status(status) = client.call(&Request::Status).await.unwrap() else {
                panic!("status")
            };
            if status.locked && !status.locking {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "세션 잠금 응답 시간 초과"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!drive.exists());
        assert_eq!(
            std::fs::read(root.join("pending.txt")).unwrap(),
            b"session-safe"
        );
        send_session_event(pid, WTS_SESSION_UNLOCK);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let Response::Status(status) = client.call(&Request::Status).await.unwrap() else {
            panic!("status")
        };
        assert!(
            status.locked,
            "Windows 잠금 해제는 PIN 잠금을 해제하지 않습니다"
        );
    });
    assert!(env.run(&["stop"]).ok);
}

#[test]
fn pin_lock_drains_uploads_blocks_mount_and_survives_restart() {
    use expandrv_ipc::{ErrorCode, Pin, Request, Response};
    if expandrv_winfsp::runtime::runtime_dll().is_none() {
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let env = Env::new("pin-lock");
    let _keys = CacheKeys::new(&["locked-test"]);
    let _pin = PinGuard(expandrv_secrets::lock::PinStore::new(&env.path("config.toml")).unwrap());
    let root = env.path("remote");
    std::fs::create_dir(&root).unwrap();
    let letter = free_drives(1).remove(0);
    let drive = PathBuf::from(format!("{letter}\\"));
    assert!(
        env.run(&[
            "add",
            "locked-test",
            &letter,
            "local",
            root.to_str().unwrap()
        ])
        .ok
    );
    assert!(env.run(&["mount", "locked-test"]).ok);
    let _guard = AgentGuard {
        env: &env,
        pid: env.agent_pid().unwrap(),
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mut client = IpcClient::connect_to(&env.pipe, Duration::from_secs(2))
            .await
            .unwrap();
        client.hello().await.unwrap();
        assert!(matches!(
            client.call(&Request::Lock).await.unwrap(),
            Response::Error {
                code: ErrorCode::Config,
                ..
            }
        ));
        assert_eq!(
            client
                .call(&Request::SetPin {
                    current: Pin(String::new()),
                    new: Pin("123456".into())
                })
                .await
                .unwrap(),
            Response::Ok
        );
        std::fs::write(drive.join("pending.txt"), b"save before lock").unwrap();
        assert_eq!(client.call(&Request::Lock).await.unwrap(), Response::Ok);
        assert!(!drive.exists());
        assert!(!env.run(&["check", "locked-test"]).ok);
        assert_eq!(
            std::fs::read(root.join("pending.txt")).unwrap(),
            b"save before lock"
        );
        let Response::Status(status) = client.call(&Request::Status).await.unwrap() else {
            panic!("status")
        };
        assert!(status.locked && status.pin_configured && !status.locking);
        assert!(status.drives.iter().all(|d| d.pending_uploads == 0));
        assert!(matches!(
            client
                .call(&Request::Mount {
                    id: "locked-test".into()
                })
                .await
                .unwrap(),
            Response::Error {
                code: ErrorCode::Locked,
                ..
            }
        ));
        assert!(matches!(
            client
                .call(&Request::Unlock {
                    pin: Pin("654321".into())
                })
                .await
                .unwrap(),
            Response::Error {
                code: ErrorCode::Authentication,
                ..
            }
        ));
        assert_eq!(
            client
                .call(&Request::Unlock {
                    pin: Pin("123456".into())
                })
                .await
                .unwrap(),
            Response::Ok
        );
        assert!(!drive.exists(), "잠금 해제는 자동 마운트하지 않습니다");
    });
    assert!(env.run(&["mount", "locked-test"]).ok);
    assert!(drive.exists());
    rt.block_on(async {
        let mut client = IpcClient::connect_to(&env.pipe, Duration::from_secs(2))
            .await
            .unwrap();
        client.hello().await.unwrap();
        assert_eq!(client.call(&Request::Lock).await.unwrap(), Response::Ok);
        for _ in 0..5 {
            assert!(matches!(
                client
                    .call(&Request::Unlock {
                        pin: Pin("654321".into())
                    })
                    .await
                    .unwrap(),
                Response::Error {
                    code: ErrorCode::Authentication,
                    ..
                }
            ));
        }
        assert!(matches!(
            client
                .call(&Request::Unlock {
                    pin: Pin("123456".into())
                })
                .await
                .unwrap(),
            Response::Error {
                code: ErrorCode::Busy,
                ..
            }
        ));
    });
    assert!(env.run(&["stop"]).ok);
    // 재시작으로 PIN 잠금을 건너뛸 수 없습니다.
    assert!(!env.run(&["mount", "locked-test"]).ok);
    let _restarted = AgentGuard {
        env: &env,
        pid: env.agent_pid().unwrap(),
    };
    assert!(!drive.exists());
    assert!(env.run(&["stop"]).ok);
}

#[test]
fn agent_mount_list_unmount_stop() {
    if expandrv_winfsp::runtime::runtime_dll().is_none() {
        eprintln!("WinFsp 런타임 미설치: 건너뜀");
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let env = Env::new("main");
    let _keys = CacheKeys::new(&["t1", "bad"]);
    let letters = free_drives(2);
    let (mp, bad_mp) = (letters[0].as_str(), letters[1].as_str());
    let root = env.path("remote");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("existing.txt"), b"hello").unwrap();
    let root_s = root.to_str().unwrap();
    let missing = env.path("missing");

    assert!(env.run(&["add", "t1", mp, "local", root_s]).ok);
    assert!(
        env.run(&["add", "bad", bad_mp, "local", missing.to_str().unwrap()])
            .ok
    );

    // 에이전트가 없으면 상태는 "-"
    let r = env.run(&["list"]);
    assert!(r.ok);
    assert!(line_of(&r.stdout, "t1").contains(" - "), "{}", r.stdout);
    assert!(!env.run(&["unmount", "t1"]).ok);

    // mount가 백그라운드 에이전트를 띄웁니다.
    let r = env.run(&["mount", "t1"]);
    assert!(r.ok, "{}", r.stderr);
    assert!(r.stdout.contains(&format!("t1 -> {mp}")), "{}", r.stdout);
    let pid = env.agent_pid().expect("에이전트가 응답하지 않습니다");
    let _guard = AgentGuard { env: &env, pid };
    assert_ne!(pid, std::process::id());
    let drive = PathBuf::from(format!("{mp}\\"));
    assert!(drive.exists());
    assert_eq!(std::fs::read(drive.join("existing.txt")).unwrap(), b"hello");

    // 두 번째 에이전트는 시작을 거부합니다.
    let r = env.run(&["agent"]);
    assert!(!r.ok);
    assert!(r.stderr.contains("이미 실행 중"), "{}", r.stderr);
    assert_eq!(env.agent_pid(), Some(pid));

    // 한 드라이브의 실패는 실제 오류 메시지로 돌아오고 다른 드라이브에 영향이 없습니다.
    let r = env.run(&["mount", "bad"]);
    assert!(!r.ok);
    assert!(r.stderr.contains("로컬 루트"), "{}", r.stderr);
    let r = env.run(&["list"]);
    assert!(r.ok);
    assert!(
        line_of(&r.stdout, "t1").contains("마운트됨"),
        "{}",
        r.stdout
    );
    assert!(line_of(&r.stdout, "bad").contains("실패"), "{}", r.stdout);
    assert!(drive.exists());

    // 다시 마운트해도 오류가 아닙니다.
    let r = env.run(&["mount", "t1"]);
    assert!(r.ok);
    assert!(r.stdout.contains("이미 마운트"), "{}", r.stdout);

    // 드라이브에 쓴 파일은 해제 후 원격(로컬 폴더)에 도착해 있어야 합니다.
    let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(drive.join("written.bin"), &data).unwrap();
    let r = env.run(&["unmount", "t1"]);
    assert!(r.ok, "{}", r.stderr);
    assert_eq!(std::fs::read(root.join("written.bin")).unwrap(), data);
    assert!(!drive.exists());
    assert!(!env.run(&["unmount", "t1"]).ok);
    let r = env.run(&["list"]);
    assert!(line_of(&r.stdout, "t1").contains(" - "), "{}", r.stdout);

    // stop은 에이전트가 끝날 때까지 기다립니다.
    let r = env.run(&["stop"]);
    assert!(r.ok, "{}", r.stderr);
    assert!(wait_exit(pid, Duration::from_secs(10)));
    assert_eq!(env.agent_pid(), None);
    assert!(env.run(&["stop"]).ok);

    // 분리 실행한 에이전트는 파일로 로그를 남깁니다.
    let logs: Vec<_> = std::fs::read_dir(env.path("localappdata/ExpanDrv/logs"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(
        logs.iter()
            .any(|n| n.starts_with("agent.") && n.ends_with(".log")),
        "{logs:?}"
    );
}

#[test]
fn ask_password_stores_credential_and_remove_cleans_up() {
    use expandrv_secrets::CredentialStore;
    let env = Env::new("secret");
    let id = format!("itest{}", std::process::id());
    let target = format!("ExpanDrv/{id}/password");
    let secret = format!("pw-{}-한글", std::process::id());

    let r = env.run_with_input(
        &[
            "add",
            &id,
            "Q:",
            "webdav",
            "https://dav.example.com/",
            "--user",
            "u",
            "--ask-password",
        ],
        &secret,
    );
    assert!(r.ok, "{}{}", r.stdout, r.stderr);
    let store = CredentialStore;
    let cleanup = scopeguard(&target);
    let cfg = std::fs::read_to_string(env.path("config.toml")).unwrap();
    assert!(
        cfg.contains(r#"from = "credential""#) && cfg.contains(&target),
        "{cfg}"
    );
    assert!(!cfg.contains(&secret));
    {
        use secrecy::ExposeSecret;
        assert_eq!(store.get(&target).unwrap().expose_secret(), secret);
    }

    let r = env.run(&["secret", "list", &id]);
    assert!(r.ok && r.stdout.contains(&target), "{}", r.stdout);
    assert!(!r.stdout.contains(&secret));

    // 빈 입력은 저장하지 않고 설정도 바꾸지 않습니다.
    let r = env.run_with_input(
        &[
            "add",
            "empty",
            "R:",
            "webdav",
            "https://dav.example.com/",
            "--user",
            "u",
            "--ask-password",
        ],
        "",
    );
    assert!(!r.ok);
    assert!(
        !std::fs::read_to_string(env.path("config.toml"))
            .unwrap()
            .contains("empty")
    );

    let r = env.run(&["remove", &id]);
    assert!(r.ok, "{}", r.stderr);
    assert!(matches!(
        store.get(&target),
        Err(expandrv_core::Error::NotFound)
    ));
    drop(cleanup);
}

/// 테스트가 실패해도 저장한 자격 증명을 지웁니다.
fn scopeguard(target: &str) -> impl Drop + '_ {
    struct G<'a>(&'a str);
    impl Drop for G<'_> {
        fn drop(&mut self) {
            let _ = expandrv_secrets::CredentialStore.delete(self.0);
        }
    }
    G(target)
}

#[test]
fn autostart_agent_mounts_only_auto_mount_drives() {
    if expandrv_winfsp::runtime::runtime_dll().is_none() {
        eprintln!("WinFsp 런타임 미설치: 건너뜀");
        return;
    }
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let env = Env::new("auto");
    let _keys = CacheKeys::new(&["auto", "manual"]);
    let letters = free_drives(2);
    let (auto_mp, manual_mp) = (letters[0].as_str(), letters[1].as_str());
    let root = env.path("remote");
    std::fs::create_dir(&root).unwrap();
    let root_s = root.to_str().unwrap();
    assert!(
        env.run(&["add", "auto", auto_mp, "--auto-mount", "local", root_s])
            .ok
    );
    assert!(env.run(&["add", "manual", manual_mp, "local", root_s]).ok);

    let mut child = env
        .command(&["agent", "--autostart"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let guard_pid = child.id();
    let _guard = AgentGuard {
        env: &env,
        pid: guard_pid,
    };
    let drive = PathBuf::from(format!("{auto_mp}\\"));
    for _ in 0..100 {
        if drive.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(drive.exists(), "자동 마운트 드라이브가 나타나지 않았습니다");
    assert!(!Path::new(&format!("{manual_mp}\\")).exists());
    let r = env.run(&["list"]);
    assert!(
        line_of(&r.stdout, "auto").contains("마운트됨"),
        "{}",
        r.stdout
    );
    assert!(line_of(&r.stdout, "manual").contains(" - "), "{}", r.stdout);

    assert!(env.run(&["stop"]).ok);
    assert!(child.wait().unwrap().success());
    assert!(!drive.exists());
}
