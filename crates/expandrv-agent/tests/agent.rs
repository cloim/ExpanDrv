//! `expandrv` 바이너리로 상주 에이전트의 mount, list, unmount, stop 흐름을 확인합니다.
//! WinFsp 런타임이 없으면 건너뜁니다.

use expandrv_ipc::IpcClient;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};

const CMD_TIMEOUT: Duration = Duration::from_secs(120);

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
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("localappdata")).unwrap();
        Self {
            pipe: format!(r"\\.\pipe\expandrv-itest-{}", std::process::id()),
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

#[test]
fn agent_mount_list_unmount_stop() {
    if expandrv_winfsp::runtime::runtime_dll().is_none() {
        eprintln!("WinFsp 런타임 미설치: 건너뜀");
        return;
    }
    let env = Env::new();
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
