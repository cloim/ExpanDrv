use crate::{Error, RemotePath, Result};
use bytes::Bytes;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    File,
    Dir,
}

/// 원격 항목 메타데이터. 백엔드가 모르는 값은 `None`으로 둡니다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// 마지막 구성 요소 이름. 루트는 빈 문자열입니다.
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    /// 원격 변경 감지용 버전 표식(ETag, 수정 시각과 크기 조합 등)
    pub etag: Option<String>,
    pub readonly: bool,
}

impl Entry {
    pub fn dir(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: EntryKind::Dir,
            size: 0,
            modified: None,
            created: None,
            etag: None,
            readonly: false,
        }
    }

    pub fn file(name: impl Into<String>, size: u64) -> Self {
        Self {
            kind: EntryKind::File,
            size,
            ..Self::dir(name)
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }
}

/// 업로드 원본. 큰 파일은 VFS의 스테이징 파일 경로로 넘겨 백엔드가 스트리밍하거나 청크로 나눠 올립니다.
#[derive(Debug, Clone)]
pub enum WriteSource {
    Bytes(Bytes),
    File { path: PathBuf, size: u64 },
}

/// 스트리밍 업로드를 시작할 때 백엔드에 주는 힌트. 탐색기는 데이터를 쓰기 전에 최종 크기를 설정하므로
/// 크기를 알면 `Content-Length`나 파트 크기 결정에 씁니다.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UploadHint {
    pub size: Option<u64>,
    /// 원격에 아직 없다고 알려진 새 파일. 백엔드는 대상 존재 확인 같은 왕복을 생략할 수 있습니다.
    pub new_file: bool,
}

/// 호출 프로그램이 데이터를 쓰는 동안 원격으로 바로 보내는 업로드 세션.
///
/// 구현 규칙:
/// - `finish` 전에는 대상 경로의 기존 내용을 바꾸지 않는 것이 바람직합니다(임시 파일 뒤 이름 변경 등).
/// - `random_write` 능력이 없는 백엔드는 `write`의 `offset`이 지금까지 보낸 바이트 수와 같다고 가정해도 됩니다.
///   다르면 [`Error::Unsupported`]를 돌려주고, VFS는 닫을 때 전체 업로드로 전환합니다.
/// - `finish`와 `abort`는 한 번만 호출됩니다. 세션이 그냥 버려지면 임시 자원을 최선으로 정리합니다.
#[async_trait::async_trait]
pub trait UploadSession: Send {
    async fn write(&mut self, offset: u64, data: Bytes) -> Result<()>;

    /// 지금까지 보낸 데이터가 원격에 수락될 때까지 기다립니다(FlushFileBuffers 대응).
    async fn flush(&mut self) -> Result<()>;

    /// 대상 경로에 확정하고 최종 항목을 돌려줍니다.
    async fn finish(self: Box<Self>) -> Result<Entry>;

    async fn abort(self: Box<Self>);
}

impl WriteSource {
    pub fn len(&self) -> u64 {
        match self {
            WriteSource::Bytes(b) => b.len() as u64,
            WriteSource::File { size, .. } => *size,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Quota {
    pub total: Option<u64>,
    pub used: Option<u64>,
    pub free: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// `read_range`가 실제 부분 읽기를 수행하는지 여부. false면 VFS가 전체 다운로드 후 캐시합니다.
    pub range_read: bool,
    pub set_mtime: bool,
    /// `rename`이 디렉터리를 원자적으로 옮길 수 있는지 여부
    pub rename_dir: bool,
    pub case_sensitive: bool,
    /// 부분 읽기를 중간에 끊는 비용이 커서 연속 블록을 하나의 전송으로 이어 받는 백엔드인지 여부.
    /// true면 VFS가 선행 읽기 블록을 동시에 요청하지 않고 순서대로 요청합니다.
    pub sequential_transfer: bool,
    /// [`Backend::begin_upload`] 스트리밍 세션을 제공하는지 여부
    pub streaming_upload: bool,
    /// 스트리밍 세션이 임의 오프셋 쓰기를 받는지 여부. false면 순차 쓰기만 스트리밍합니다.
    pub random_write: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            range_read: true,
            set_mtime: false,
            rename_dir: true,
            case_sensitive: false,
            sequential_transfer: false,
            streaming_upload: false,
            random_write: false,
        }
    }
}

/// 원격 스토리지 추상화. 모든 경로는 드라이브 루트 기준 [`RemotePath`]입니다.
///
/// 구현 규칙:
/// - 없는 항목은 [`Error::NotFound`], 인증 실패는 [`Error::Auth`]로 돌려줍니다.
/// - `read_range`는 EOF에서 요청보다 짧은 데이터를 돌려줄 수 있고, 오프셋이 EOF 이상이면 빈 값입니다.
/// - `write`는 없으면 만들고 있으면 덮어씁니다. 부모 디렉터리는 이미 있다고 가정합니다.
/// - `rename`에서 `replace == false`이고 대상이 있으면 [`Error::AlreadyExists`]입니다.
/// - `delete`의 디렉터리 삭제는 빈 디렉터리만 대상으로 합니다.
#[async_trait::async_trait]
pub trait Backend: Send + Sync + 'static {
    /// 로그와 진단용 백엔드 종류 이름
    fn kind(&self) -> &'static str;

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    async fn stat(&self, path: &RemotePath) -> Result<Entry>;

    async fn list(&self, path: &RemotePath) -> Result<Vec<Entry>>;

    async fn read_range(&self, path: &RemotePath, offset: u64, len: u64) -> Result<Bytes>;

    async fn write(&self, path: &RemotePath, source: WriteSource) -> Result<Entry>;

    /// 스트리밍 업로드 세션을 엽니다. `streaming_upload` 능력이 없는 백엔드는 지원하지 않습니다.
    async fn begin_upload(
        &self,
        _path: &RemotePath,
        _hint: UploadHint,
    ) -> Result<Box<dyn UploadSession>> {
        Err(Error::Unsupported("스트리밍 업로드"))
    }

    async fn mkdir(&self, path: &RemotePath) -> Result<Entry>;

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> Result<()>;

    async fn delete(&self, path: &RemotePath, kind: EntryKind) -> Result<()>;

    async fn set_mtime(&self, _path: &RemotePath, _mtime: SystemTime) -> Result<()> {
        Err(Error::Unsupported("set_mtime"))
    }

    async fn quota(&self) -> Result<Quota> {
        Ok(Quota::default())
    }
}
