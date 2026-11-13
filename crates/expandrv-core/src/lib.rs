//! ExpanDrv 공통 계층: 원격 경로, 오류 모델, `Backend` 트레이트, 설정 모델.

mod backend;
pub mod config;
mod error;
mod path;

pub use backend::{
    Backend, Capabilities, Entry, EntryKind, Quota, UploadHint, UploadSession, WriteSource,
};
pub use error::{Error, Result, ntstatus};
pub use path::RemotePath;
