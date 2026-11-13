//! 요청과 응답 형식. 호환되지 않게 바꾸면 [`PROTOCOL_VERSION`]을 올립니다.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// 연결 직후 버전을 맞춥니다.
    Hello {
        version: u32,
    },
    Status,
    Lock,
    Unlock {
        pin: Pin,
    },
    SetPin {
        current: Pin,
        new: Pin,
    },
    Mount {
        id: String,
    },
    Unmount {
        id: String,
    },
    /// 설정 파일을 다시 읽습니다. 마운트된 드라이브는 그대로 둡니다.
    Reload,
    /// 모든 드라이브를 해제(업로드 마무리 포함)하고 에이전트를 끝냅니다.
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Hello { version: u32, agent_version: String },
    Status(AgentStatus),
    Ok,
    Error { code: ErrorCode, message: String },
}

impl Response {
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Response::Error {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    BadRequest,
    Locked,
    Authentication,
    UnknownDrive,
    AlreadyMounted,
    NotMounted,
    /// 같은 드라이브에 다른 작업이 진행 중입니다.
    Busy,
    MountFailed,
    /// 해제는 했지만 남은 업로드를 마치지 못했습니다.
    UnmountFailed,
    Config,
    ShuttingDown,
    Internal,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatus {
    pub locked: bool,
    pub locking: bool,
    pub pin_configured: bool,
    pub drives: Vec<DriveStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriveStatus {
    pub id: String,
    pub label: String,
    pub mount_point: String,
    pub backend: String,
    pub state: DriveState,
    /// 업로드가 끝나지 않은 파일 전체 수(전송 중, 대기, 실패 합계)
    pub pending_uploads: u64,
    /// 원격에서 받고 있는 블록 수. 이전 에이전트 응답에는 없을 수 있습니다.
    #[serde(default)]
    pub active_downloads: u64,
    /// `pending_uploads` 중 지금 전송 중이거나 닫을 때 바로 반영할 파일 수
    #[serde(default)]
    pub uploading: u64,
    /// `pending_uploads` 중 마지막 시도가 실패해 다시 시도를 기다리는 파일 수
    #[serde(default)]
    pub failed_uploads: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriveState {
    Mounted,
    Unmounted,
    /// 마지막 마운트 또는 해제 실패 사유
    Failed(String),
}

/// PIN은 IPC Debug 출력에서 가리고 소유권 해제 시 메모리를 지웁니다.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Pin(pub String);
impl std::fmt::Debug for Pin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[PIN]")
    }
}
impl Drop for Pin {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pin_is_hidden_from_request_debug() {
        let req = Request::Unlock {
            pin: Pin("123456".into()),
        };
        assert!(!format!("{req:?}").contains("123456"));
        let req = Request::SetPin {
            current: Pin("123456".into()),
            new: Pin("654321".into()),
        };
        let debug = format!("{req:?}");
        assert!(!debug.contains("123456") && !debug.contains("654321"));
    }
}
