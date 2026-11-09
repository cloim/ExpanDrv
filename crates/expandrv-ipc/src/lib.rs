//! 에이전트 제어용 IPC.
//!
//! 현재 사용자만 열 수 있는 이름 있는 파이프(`\\.\pipe\expandrv-<SID>`)에
//! 길이 접두(u32 LE) + MessagePack 프레임으로 [`Request`]와 [`Response`]를 주고받습니다.
//! WinFsp에 의존하지 않으므로 권한 없는 GUI 프로세스에서도 그대로 씁니다.

mod frame;
mod pipe;
mod proto;
mod sid;

pub use frame::{MAX_FRAME, read_frame, write_frame};
pub use pipe::{Handler, IpcClient, IpcServer, default_pipe_name};
pub use proto::{
    AgentStatus, DriveState, DriveStatus, ErrorCode, PROTOCOL_VERSION, Pin, Request, Response,
};
pub use sid::{current_user_sid, process_user_sid};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("에이전트가 실행 중이 아닙니다")]
    NotRunning,
    #[error("에이전트가 이미 실행 중입니다")]
    AlreadyRunning,
    #[error("에이전트 연결 시간이 초과되었습니다")]
    Timeout,
    #[error("파이프 상대 프로세스가 현재 사용자 소유가 아닙니다")]
    PeerMismatch,
    #[error("프레임이 너무 큽니다: {0}바이트")]
    FrameTooLarge(usize),
    #[error("메시지 형식 오류: {0}")]
    Codec(String),
    #[error("연결이 닫혔습니다")]
    Closed,
    #[error("프로토콜 버전이 맞지 않습니다: 에이전트 {agent}, 클라이언트 {client}")]
    VersionMismatch { agent: u32, client: u32 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
