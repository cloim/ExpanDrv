//! 상주 에이전트 연결과 시작, 종료. 에이전트는 CLI(`expandrv agent --background`)를 분리 실행해 띄웁니다.

use expandrv_ipc::{AgentStatus, ErrorCode, IpcClient, Request, Response};
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 새로 띄운 에이전트가 응답할 때까지 기다리는 시간
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// 실행 중인 에이전트의 모든 인스턴스가 잠시 바쁠 때 기다리는 시간
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);
/// 종료 요청 뒤 프로세스가 파이프를 닫을 때까지 기다리는 시간
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const ERROR_ACCESS_DENIED: i32 = 5;

pub type Result<T> = std::result::Result<T, String>;

fn ipc(e: expandrv_ipc::Error) -> String {
    e.to_string()
}

pub struct Link {
    pub client: IpcClient,
    pub version: String,
}

impl Link {
    async fn open(mut client: IpcClient) -> Result<Self> {
        let version = client.hello().await.map_err(ipc)?;
        Ok(Self { client, version })
    }

    pub async fn call(&mut self, req: &Request) -> Result<Response> {
        self.client.call(req).await.map_err(ipc)
    }

    /// 응답이 오류면 에이전트 메시지를 그대로 돌려줍니다.
    pub async fn call_ok(&mut self, req: &Request) -> Result<()> {
        match self.call(req).await? {
            Response::Ok => Ok(()),
            Response::Error { message, .. } => Err(message),
            other => Err(format!("예상하지 못한 응답: {other:?}")),
        }
    }

    pub async fn status(&mut self) -> Result<AgentStatus> {
        match self.call(&Request::Status).await? {
            Response::Status(s) => Ok(s),
            Response::Error { message, .. } => Err(message),
            other => Err(format!("예상하지 못한 응답: {other:?}")),
        }
    }

    /// 마운트합니다. 이미 마운트되어 있으면 성공으로 봅니다.
    pub async fn mount(&mut self, id: &str) -> Result<()> {
        match self.call(&Request::Mount { id: id.to_owned() }).await? {
            Response::Ok
            | Response::Error {
                code: ErrorCode::AlreadyMounted,
                ..
            } => Ok(()),
            Response::Error { message, .. } => Err(message),
            other => Err(format!("예상하지 못한 응답: {other:?}")),
        }
    }
}

/// 실행 중인 에이전트에 연결합니다. 없으면 `None`입니다.
pub async fn connect(pipe: &str) -> Result<Option<Link>> {
    let r = match IpcClient::connect_to(pipe, Duration::ZERO).await {
        Err(expandrv_ipc::Error::Timeout) => IpcClient::connect_to(pipe, BUSY_TIMEOUT).await,
        r => r,
    };
    match r {
        Ok(client) => Link::open(client).await.map(Some),
        Err(expandrv_ipc::Error::NotRunning) => Ok(None),
        Err(e) => Err(ipc(e)),
    }
}

/// 에이전트가 있어야 하는 작업용 연결
pub async fn require(pipe: &str) -> Result<Link> {
    connect(pipe)
        .await?
        .ok_or_else(|| expandrv_ipc::Error::NotRunning.to_string())
}

/// 연결하고, 에이전트가 없으면 CLI로 백그라운드 에이전트를 띄운 뒤 응답할 때까지 기다립니다.
pub async fn start(pipe: &str, cfg_path: &Path, cli: &Path, log_dir: &Path) -> Result<Link> {
    if let Some(link) = connect(pipe).await? {
        return Ok(link);
    }
    let mut child = spawn(pipe, cfg_path, cli)?;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match IpcClient::connect_to(pipe, Duration::from_millis(200)).await {
            Ok(client) => return Link::open(client).await,
            Err(expandrv_ipc::Error::NotRunning | expandrv_ipc::Error::Timeout) => {}
            Err(e) => return Err(ipc(e)),
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "에이전트가 시작하지 못했습니다({status}). 로그: {}",
                log_dir.display()
            ));
        }
        if Instant::now() >= deadline {
            return Err("에이전트가 제한 시간 안에 응답하지 않았습니다".into());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// 콘솔 창 없이, GUI와 분리된 프로세스 그룹으로 에이전트를 띄웁니다. GUI를 닫아도 에이전트는 남습니다.
fn spawn(pipe: &str, cfg_path: &Path, cli: &Path) -> Result<std::process::Child> {
    if !cli.is_file() {
        return Err(format!(
            "CLI 실행 파일이 없습니다: {}. GUI와 같은 폴더에 expandrv.exe를 두세요",
            cli.display()
        ));
    }
    let cfg_path = std::path::absolute(cfg_path).map_err(|e| e.to_string())?;
    let mut cmd = Command::new(cli);
    cmd.arg("--config")
        .arg(&cfg_path)
        .arg("--pipe")
        .arg(pipe)
        .args(["agent", "--background"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(dir) = cli.parent() {
        cmd.current_dir(dir);
    }
    let flags = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
    // GUI가 작업 개체 안에 있으면 함께 종료되지 않도록 벗어나 봅니다.
    match cmd
        .creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
    {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
            cmd.creation_flags(flags).spawn().map_err(|e| e.to_string())
        }
        r => r.map_err(|e| format!("에이전트를 시작하지 못했습니다: {e}")),
    }
}

/// 모든 드라이브를 해제하고 에이전트를 끝냅니다. 남은 업로드가 끝난 뒤에 응답이 옵니다.
pub async fn stop(pipe: &str) -> Result<bool> {
    let Some(mut link) = connect(pipe).await? else {
        return Ok(false);
    };
    let r = link.call_ok(&Request::Shutdown).await;
    drop(link);
    let deadline = Instant::now() + EXIT_TIMEOUT;
    loop {
        match IpcClient::connect_to(pipe, Duration::ZERO).await {
            Err(expandrv_ipc::Error::NotRunning) => break,
            _ if Instant::now() >= deadline => {
                return Err("에이전트가 아직 종료되지 않았습니다".into());
            }
            _ => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
    r.map(|()| true)
}
