use std::io;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// 백엔드와 VFS 공통 오류. 메시지에는 자격 증명을 넣지 않습니다.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("항목을 찾을 수 없습니다")]
    NotFound,
    #[error("항목이 이미 있습니다")]
    AlreadyExists,
    #[error("디렉터리가 아닙니다")]
    NotADirectory,
    #[error("파일이 아니라 디렉터리입니다")]
    IsADirectory,
    #[error("디렉터리가 비어 있지 않습니다")]
    DirectoryNotEmpty,
    #[error("권한이 없습니다")]
    PermissionDenied,
    #[error("읽기 전용 드라이브입니다")]
    ReadOnly,
    #[error("잘못된 경로: {0}")]
    InvalidPath(String),
    #[error("지원하지 않는 작업: {0}")]
    Unsupported(&'static str),
    #[error("인증 실패: {0}")]
    Auth(String),
    #[error("시간 초과")]
    Timeout,
    #[error("작업이 취소되었습니다")]
    Cancelled,
    #[error("저장 공간이 부족합니다")]
    StorageFull,
    #[error("네트워크 오류: {0}")]
    Network(String),
    #[error("프로토콜 오류: {0}")]
    Protocol(String),
    #[error("설정 오류: {0}")]
    Config(String),
    #[error(transparent)]
    Io(io::Error),
}

impl From<io::Error> for Error {
    /// 파일 시스템 의미가 있는 I/O 오류는 전용 변형으로 옮깁니다.
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::NotFound => Error::NotFound,
            io::ErrorKind::AlreadyExists => Error::AlreadyExists,
            io::ErrorKind::PermissionDenied => Error::PermissionDenied,
            io::ErrorKind::NotADirectory => Error::NotADirectory,
            io::ErrorKind::IsADirectory => Error::IsADirectory,
            io::ErrorKind::DirectoryNotEmpty => Error::DirectoryNotEmpty,
            io::ErrorKind::StorageFull => Error::StorageFull,
            io::ErrorKind::TimedOut => Error::Timeout,
            io::ErrorKind::ReadOnlyFilesystem => Error::ReadOnly,
            _ => Error::Io(e),
        }
    }
}

/// WinFsp에 돌려줄 NTSTATUS 값. `windows` 크레이트에 의존하지 않도록 값을 직접 둡니다.
pub mod ntstatus {
    pub const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034_u32 as i32;
    pub const STATUS_OBJECT_PATH_NOT_FOUND: i32 = 0xC000_003A_u32 as i32;
    pub const STATUS_OBJECT_NAME_COLLISION: i32 = 0xC000_0035_u32 as i32;
    pub const STATUS_NOT_A_DIRECTORY: i32 = 0xC000_0103_u32 as i32;
    pub const STATUS_FILE_IS_A_DIRECTORY: i32 = 0xC000_00BA_u32 as i32;
    pub const STATUS_DIRECTORY_NOT_EMPTY: i32 = 0xC000_0101_u32 as i32;
    pub const STATUS_ACCESS_DENIED: i32 = 0xC000_0022_u32 as i32;
    pub const STATUS_MEDIA_WRITE_PROTECTED: i32 = 0xC000_00A2_u32 as i32;
    pub const STATUS_OBJECT_NAME_INVALID: i32 = 0xC000_0033_u32 as i32;
    pub const STATUS_NOT_SUPPORTED: i32 = 0xC000_00BB_u32 as i32;
    pub const STATUS_LOGON_FAILURE: i32 = 0xC000_006D_u32 as i32;
    pub const STATUS_IO_TIMEOUT: i32 = 0xC000_00B5_u32 as i32;
    pub const STATUS_CANCELLED: i32 = 0xC000_0120_u32 as i32;
    pub const STATUS_DISK_FULL: i32 = 0xC000_007F_u32 as i32;
    pub const STATUS_NETWORK_UNREACHABLE: i32 = 0xC000_023C_u32 as i32;
    pub const STATUS_UNEXPECTED_NETWORK_ERROR: i32 = 0xC000_00C4_u32 as i32;
    pub const STATUS_INVALID_PARAMETER: i32 = 0xC000_000D_u32 as i32;
    pub const STATUS_UNEXPECTED_IO_ERROR: i32 = 0xC000_00E9_u32 as i32;
}

impl Error {
    pub fn ntstatus(&self) -> i32 {
        use ntstatus::*;
        match self {
            Error::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
            Error::AlreadyExists => STATUS_OBJECT_NAME_COLLISION,
            Error::NotADirectory => STATUS_NOT_A_DIRECTORY,
            Error::IsADirectory => STATUS_FILE_IS_A_DIRECTORY,
            Error::DirectoryNotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
            Error::PermissionDenied => STATUS_ACCESS_DENIED,
            Error::ReadOnly => STATUS_MEDIA_WRITE_PROTECTED,
            Error::InvalidPath(_) => STATUS_OBJECT_NAME_INVALID,
            Error::Unsupported(_) => STATUS_NOT_SUPPORTED,
            Error::Auth(_) => STATUS_LOGON_FAILURE,
            Error::Timeout => STATUS_IO_TIMEOUT,
            Error::Cancelled => STATUS_CANCELLED,
            Error::StorageFull => STATUS_DISK_FULL,
            Error::Network(_) => STATUS_NETWORK_UNREACHABLE,
            Error::Protocol(_) => STATUS_UNEXPECTED_NETWORK_ERROR,
            Error::Config(_) => STATUS_INVALID_PARAMETER,
            Error::Io(_) => STATUS_UNEXPECTED_IO_ERROR,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_errors_map_to_fs_variants() {
        let e: Error = io::Error::from(io::ErrorKind::NotFound).into();
        assert!(matches!(e, Error::NotFound));
        assert_eq!(e.ntstatus(), ntstatus::STATUS_OBJECT_NAME_NOT_FOUND);
        let e: Error = io::Error::other("x").into();
        assert_eq!(e.ntstatus(), ntstatus::STATUS_UNEXPECTED_IO_ERROR);
    }
}
