//! CLI 쪽 에이전트 연결: 연결 확인, 필요하면 백그라운드 에이전트 시작, 동기 호출

use crate::{ipc_err, log_dir, message};
use expandrv_core::{Error, Result};
use expandrv_ipc::{AgentStatus, IpcClient, Request, Response};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, OpenProcess,
    PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

/// 새로 띄운 에이전트가 응답할 때까지 기다리는 시간
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// 실행 중인 에이전트의 모든 인스턴스가 잠시 바쁠 때 기다리는 시간
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);
const ERROR_ACCESS_DENIED: i32 = 5;

pub struct AgentLink {
    rt: tokio::runtime::Runtime,
    client: IpcClient,
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

/// 응답이 오류면 메시지를 그대로 CLI 오류로 바꿉니다.
pub fn expect_ok(resp: Response) -> Result<()> {
    match resp {
        Response::Ok => Ok(()),
        Response::Error { message: m, .. } => Err(message(m)),
        other => Err(Error::Protocol(format!("예상하지 못한 응답: {other:?}"))),
    }
}

impl AgentLink {
    /// 실행 중인 에이전트에 연결합니다. 없으면 `None`입니다.
    pub fn connect(pipe: &str) -> Result<Option<Self>> {
        let rt = runtime()?;
        let r = rt.block_on(async {
            match IpcClient::connect_to(pipe, Duration::ZERO).await {
                Err(expandrv_ipc::Error::Timeout) => {
                    IpcClient::connect_to(pipe, BUSY_TIMEOUT).await
                }
                r => r,
            }
        });
        match r {
            Ok(client) => {
                let mut link = Self { rt, client };
                link.hello()?;
                Ok(Some(link))
            }
            Err(expandrv_ipc::Error::NotRunning) => Ok(None),
            Err(e) => Err(ipc_err(e)),
        }
    }

    /// 연결하고, 에이전트가 없으면 백그라운드로 띄운 뒤 응답할 때까지 기다립니다.
    pub fn start(pipe: &str, cfg_path: &Path) -> Result<Self> {
        if let Some(link) = Self::connect(pipe)? {
            return Ok(link);
        }
        let mut child = spawn_agent(pipe, cfg_path)?;
        let deadline = Instant::now() + START_TIMEOUT;
        let rt = runtime()?;
        loop {
            match rt.block_on(IpcClient::connect_to(pipe, Duration::from_millis(200))) {
                Ok(client) => {
                    let mut link = Self { rt, client };
                    link.hello()?;
                    return Ok(link);
                }
                Err(expandrv_ipc::Error::NotRunning | expandrv_ipc::Error::Timeout) => {}
                Err(e) => return Err(ipc_err(e)),
            }
            if let Some(status) = child.try_wait()? {
                return Err(message(format!(
                    "에이전트가 시작하지 못했습니다({status}). 로그: {}",
                    log_dir()
                        .map(|d| d.display().to_string())
                        .unwrap_or_default()
                )));
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout);
            }
        }
    }

    fn hello(&mut self) -> Result<()> {
        self.rt
            .block_on(self.client.hello())
            .map(drop)
            .map_err(ipc_err)
    }

    pub fn call(&mut self, req: &Request) -> Result<Response> {
        self.rt.block_on(self.client.call(req)).map_err(ipc_err)
    }

    pub fn status(&mut self) -> Result<AgentStatus> {
        match self.call(&Request::Status)? {
            Response::Status(s) => Ok(s),
            other => expect_ok(other).and(Ok(AgentStatus::default())),
        }
    }

    pub fn pid(&self) -> u32 {
        self.client.server_pid()
    }
}

/// 상속 가능한 표준 핸들(호출자의 파이프 등)이 상주 에이전트에 새지 않게 합니다.
fn stop_std_handle_inheritance() {
    let handles = [
        std::io::stdin().as_raw_handle(),
        std::io::stdout().as_raw_handle(),
        std::io::stderr().as_raw_handle(),
    ];
    for h in handles.into_iter().filter(|h| !h.is_null()) {
        let _ = unsafe { SetHandleInformation(HANDLE(h), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) };
    }
}

/// 콘솔 창 없이, 호출한 콘솔과 분리된 프로세스 그룹으로 에이전트를 띄웁니다.
fn spawn_agent(pipe: &str, cfg_path: &Path) -> Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    let cfg_path = std::path::absolute(cfg_path)?;
    let mut cmd = Command::new(&exe);
    cmd.arg("--config")
        .arg(&cfg_path)
        .arg("--pipe")
        .arg(pipe)
        .args(["agent", "--background"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // 작업 디렉터리를 잡아 두지 않도록 실행 파일 폴더에서 시작합니다.
    if let Some(dir) = exe.parent() {
        cmd.current_dir(dir);
    }
    stop_std_handle_inheritance();
    let flags = CREATE_NO_WINDOW.0 | CREATE_NEW_PROCESS_GROUP.0;
    // 호출자가 작업 개체 안에 있으면 함께 종료되지 않도록 벗어나 봅니다.
    match cmd
        .creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB.0)
        .spawn()
    {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
            Ok(cmd.creation_flags(flags).spawn()?)
        }
        r => Ok(r?),
    }
}

/// 프로세스가 끝날 때까지 기다립니다. 이미 없으면 바로 true입니다.
pub fn wait_process_exit(pid: u32, timeout: Duration) -> bool {
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return true;
        };
        let r = WaitForSingleObject(h, timeout.as_millis() as u32);
        let _ = CloseHandle(h);
        r == WAIT_OBJECT_0
    }
}
