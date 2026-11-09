//! 소유자 전용 이름 있는 파이프 서버와 클라이언트

use crate::sid::{current_user_sid, process_user_sid};
use crate::{
    Error, ErrorCode, PROTOCOL_VERSION, Request, Response, Result, read_frame, write_frame,
};
use std::future::Future;
use std::os::windows::io::AsRawHandle;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::sync::watch;
use tokio::task::JoinSet;
use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};
use windows::core::HSTRING;

const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_PIPE_BUSY: i32 = 231;
const RETRY_DELAY: Duration = Duration::from_millis(50);

/// 현재 사용자 에이전트의 파이프 이름(`\\.\pipe\expandrv-<SID>`)
pub fn default_pipe_name() -> Result<String> {
    Ok(format!(r"\\.\pipe\expandrv-{}", current_user_sid()?))
}

/// 요청 처리기. `Hello`는 서버가 직접 답하므로 받지 않습니다.
#[async_trait::async_trait]
pub trait Handler: Send + Sync + 'static {
    fn agent_version(&self) -> String;
    async fn handle(&self, req: Request) -> Response;
}

/// 보호된 DACL로 현재 사용자 SID에만 접근을 허용하는 보안 서술자
struct PipeSecurity(PSECURITY_DESCRIPTOR);

// SAFETY: 생성 후 읽기만 하는 LocalAlloc 메모리입니다.
unsafe impl Send for PipeSecurity {}
unsafe impl Sync for PipeSecurity {}

impl PipeSecurity {
    fn owner_only(sid: &str) -> Result<Self> {
        let sddl = format!("O:{sid}D:P(A;;GA;;;{sid})");
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(sddl),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .map_err(|e| Error::Io(std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF)))?;
        Ok(Self(sd))
    }

    fn create(&self, opts: &ServerOptions, name: &str) -> std::io::Result<NamedPipeServer> {
        let mut sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: false.into(),
        };
        // SAFETY: sa와 보안 서술자는 호출 동안 유효합니다.
        unsafe { opts.create_with_security_attributes_raw(name, (&raw mut sa).cast()) }
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        let _ = unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
    }
}

fn same_user(pid: std::io::Result<u32>, sid: &str) -> bool {
    match pid.and_then(process_user_sid) {
        Ok(peer) => peer == sid,
        Err(e) => {
            tracing::warn!("파이프 상대 확인 실패: {e}");
            false
        }
    }
}

fn pipe_pid(
    h: &impl AsRawHandle,
    f: unsafe fn(HANDLE, *mut u32) -> windows::core::Result<()>,
) -> std::io::Result<u32> {
    let mut pid = 0u32;
    unsafe { f(HANDLE(h.as_raw_handle()), &mut pid) }
        .map_err(|e| std::io::Error::from_raw_os_error(e.code().0 & 0xFFFF))?;
    Ok(pid)
}

/// 사용자당 하나만 뜨는 IPC 서버
pub struct IpcServer {
    name: String,
    sid: String,
    security: PipeSecurity,
    first: NamedPipeServer,
}

impl IpcServer {
    /// 첫 인스턴스를 만듭니다. 같은 이름의 파이프가 이미 있으면 [`Error::AlreadyRunning`]입니다.
    pub fn bind(name: &str) -> Result<Self> {
        let sid = current_user_sid()?;
        let security = PipeSecurity::owner_only(&sid)?;
        let first = security
            .create(
                ServerOptions::new()
                    .first_pipe_instance(true)
                    .reject_remote_clients(true),
                name,
            )
            .map_err(|e| match e.raw_os_error() {
                Some(ERROR_ACCESS_DENIED) => Error::AlreadyRunning,
                _ => e.into(),
            })?;
        Ok(Self {
            name: name.to_owned(),
            sid,
            security,
            first,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// `shutdown`이 끝날 때까지 연결을 받습니다. 끝나면 대기 중인 연결을 닫고
    /// 처리 중인 요청의 응답을 보낸 뒤 돌아옵니다.
    pub async fn serve(
        self,
        handler: Arc<dyn Handler>,
        shutdown: impl Future<Output = ()>,
    ) -> Result<()> {
        let (stop_tx, stop_rx) = watch::channel(());
        let mut tasks = JoinSet::new();
        let mut server = self.first;
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                r = server.connect() => {
                    // 다음 인스턴스를 먼저 만들어 이름이 비는 순간이 없게 합니다.
                    let next = self
                        .security
                        .create(ServerOptions::new().reject_remote_clients(true), &self.name)?;
                    let conn = std::mem::replace(&mut server, next);
                    if let Err(e) = r {
                        tracing::debug!("파이프 연결 실패: {e}");
                        continue;
                    }
                    if !same_user(pipe_pid(&conn, GetNamedPipeClientProcessId), &self.sid) {
                        tracing::warn!("다른 사용자 프로세스의 연결을 거부했습니다");
                        continue;
                    }
                    tasks.spawn(serve_conn(conn, handler.clone(), stop_rx.clone()));
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
                _ = &mut shutdown => break,
            }
        }
        drop(stop_tx);
        while tasks.join_next().await.is_some() {}
        Ok(())
    }
}

async fn serve_conn(
    mut pipe: NamedPipeServer,
    handler: Arc<dyn Handler>,
    mut stop: watch::Receiver<()>,
) {
    loop {
        let req = tokio::select! {
            r = read_frame::<_, Request>(&mut pipe) => r,
            // 송신 측이 사라지면 종료 신호입니다.
            _ = stop.changed() => return,
        };
        let resp = match req {
            Ok(Some(Request::Hello { .. })) => Response::Hello {
                version: PROTOCOL_VERSION,
                agent_version: handler.agent_version(),
            },
            Ok(Some(req)) => handler.handle(req).await,
            Ok(None) => return,
            Err(Error::Codec(e)) => Response::error(ErrorCode::BadRequest, e),
            Err(e) => {
                tracing::debug!("요청 읽기 실패: {e}");
                return;
            }
        };
        let sent = match write_frame(&mut pipe, &resp).await {
            Err(Error::FrameTooLarge(n)) => {
                let resp = Response::error(
                    ErrorCode::Internal,
                    format!("응답이 너무 큽니다: {n}바이트"),
                );
                write_frame(&mut pipe, &resp).await
            }
            r => r,
        };
        if let Err(e) = sent {
            tracing::debug!("응답 쓰기 실패: {e}");
            return;
        }
    }
}

/// 에이전트 연결
pub struct IpcClient {
    pipe: NamedPipeClient,
    server_pid: u32,
}

impl IpcClient {
    /// 기본 파이프에 연결합니다. 파이프가 없거나 바쁘면 `timeout`까지 다시 시도합니다.
    pub async fn connect(timeout: Duration) -> Result<Self> {
        Self::connect_to(&default_pipe_name()?, timeout).await
    }

    pub async fn connect_to(name: &str, timeout: Duration) -> Result<Self> {
        let deadline = Instant::now() + timeout;
        let pipe = loop {
            match ClientOptions::new().open(name) {
                Ok(p) => break p,
                Err(e) => {
                    let code = e.raw_os_error();
                    if !matches!(code, Some(ERROR_FILE_NOT_FOUND | ERROR_PIPE_BUSY)) {
                        return Err(e.into());
                    }
                    if Instant::now() >= deadline {
                        return Err(match code {
                            Some(ERROR_PIPE_BUSY) => Error::Timeout,
                            _ => Error::NotRunning,
                        });
                    }
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        };
        // 다른 사용자가 먼저 만든 파이프에 붙지 않도록 서버 프로세스 소유자를 확인합니다.
        let server_pid = pipe_pid(&pipe, GetNamedPipeServerProcessId)?;
        if !same_user(Ok(server_pid), &current_user_sid()?) {
            return Err(Error::PeerMismatch);
        }
        Ok(Self { pipe, server_pid })
    }

    /// 에이전트 프로세스 id
    pub fn server_pid(&self) -> u32 {
        self.server_pid
    }

    pub async fn call(&mut self, req: &Request) -> Result<Response> {
        write_frame(&mut self.pipe, req).await?;
        read_frame(&mut self.pipe).await?.ok_or(Error::Closed)
    }

    /// 버전을 맞추고 에이전트 버전을 돌려줍니다.
    pub async fn hello(&mut self) -> Result<String> {
        let req = Request::Hello {
            version: PROTOCOL_VERSION,
        };
        match self.call(&req).await? {
            Response::Hello {
                version,
                agent_version,
            } if version == PROTOCOL_VERSION => Ok(agent_version),
            Response::Hello { version, .. } => Err(Error::VersionMismatch {
                agent: version,
                client: PROTOCOL_VERSION,
            }),
            other => Err(Error::Codec(format!("예상하지 못한 응답: {other:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentStatus, DriveState, DriveStatus};
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::sync::Notify;

    fn unique_name() -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        format!(
            r"\\.\pipe\expandrv-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        )
    }

    struct Echo {
        stop: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl Handler for Echo {
        fn agent_version(&self) -> String {
            "test".into()
        }

        async fn handle(&self, req: Request) -> Response {
            match req {
                Request::Status => Response::Status(AgentStatus {
                    drives: vec![DriveStatus {
                        id: "a".into(),
                        label: "A".into(),
                        mount_point: "Q:".into(),
                        backend: "local".into(),
                        state: DriveState::Unmounted,
                        pending_uploads: 0,
                        active_downloads: 0,
                    }],
                    ..AgentStatus::default()
                }),
                Request::Mount { id } => Response::error(ErrorCode::UnknownDrive, id),
                Request::Shutdown => {
                    self.stop.notify_one();
                    Response::Ok
                }
                _ => Response::Ok,
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn server_client_roundtrip() {
        let name = unique_name();
        let server = IpcServer::bind(&name).unwrap();
        let stop = Arc::new(Notify::new());
        let handler = Arc::new(Echo { stop: stop.clone() });
        let wait = stop.clone();
        let task = tokio::spawn(server.serve(handler, async move { wait.notified().await }));

        let mut c1 = IpcClient::connect_to(&name, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(c1.server_pid(), std::process::id());
        assert_eq!(c1.hello().await.unwrap(), "test");
        let mut c2 = IpcClient::connect_to(&name, Duration::from_secs(5))
            .await
            .unwrap();
        // 한 연결에서 여러 요청
        for _ in 0..3 {
            let Response::Status(s) = c1.call(&Request::Status).await.unwrap() else {
                panic!()
            };
            assert_eq!(s.drives[0].id, "a");
        }
        assert_eq!(
            c2.call(&Request::Mount { id: "x".into() }).await.unwrap(),
            Response::error(ErrorCode::UnknownDrive, "x")
        );
        assert_eq!(c2.call(&Request::Shutdown).await.unwrap(), Response::Ok);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        // 종료 후 대기 중이던 연결은 닫힙니다.
        assert!(c1.call(&Request::Status).await.is_err());
        assert!(matches!(
            IpcClient::connect_to(&name, Duration::ZERO).await,
            Err(Error::NotRunning)
        ));
    }

    #[tokio::test]
    async fn second_server_instance_is_refused() {
        let name = unique_name();
        let _first = IpcServer::bind(&name).unwrap();
        assert!(matches!(IpcServer::bind(&name), Err(Error::AlreadyRunning)));
    }

    #[tokio::test]
    async fn client_reports_missing_agent() {
        let started = Instant::now();
        let err = IpcClient::connect_to(&unique_name(), Duration::from_millis(200))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, Error::NotRunning));
        assert_eq!(err.to_string(), "에이전트가 실행 중이 아닙니다");
        assert!(started.elapsed() >= Duration::from_millis(200));
    }

    #[test]
    fn default_name_uses_user_sid() {
        let name = default_pipe_name().unwrap();
        assert!(name.starts_with(r"\\.\pipe\expandrv-S-1-5-"), "{name}");
    }

    #[tokio::test]
    async fn pipe_dacl_allows_only_current_user() {
        use windows::Win32::Security::Authorization::{
            ConvertSecurityDescriptorToStringSecurityDescriptorW, GetSecurityInfo, SE_KERNEL_OBJECT,
        };
        use windows::Win32::Security::{DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION};
        use windows::core::PWSTR;

        let server = IpcServer::bind(&unique_name()).unwrap();
        let info = DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION;
        let sddl = unsafe {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            GetSecurityInfo(
                HANDLE(server.first.as_raw_handle()),
                SE_KERNEL_OBJECT,
                info,
                None,
                None,
                None,
                None,
                Some(&mut sd),
            )
            .ok()
            .unwrap();
            let mut s = PWSTR::null();
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                sd,
                SDDL_REVISION_1,
                info,
                &mut s,
                None,
            )
            .unwrap();
            let out = s.to_string().unwrap();
            let _ = LocalFree(Some(HLOCAL(s.0.cast())));
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            out
        };
        // 보호된 DACL(P)에 현재 사용자 ACE 하나만 있어야 합니다.
        let sid = current_user_sid().unwrap();
        assert_eq!(sddl, format!("O:{sid}D:P(A;;FA;;;{sid})"));
    }
}
