//! 로컬 폴더 백엔드. 테스트와 VFS 검증의 기준 구현입니다.

use async_trait::async_trait;
use bytes::Bytes;
use expandrv_core::{
    Backend, Capabilities, Entry, EntryKind, Error, RemotePath, Result, WriteSource,
};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub struct LocalBackend {
    root: PathBuf,
}

impl LocalBackend {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if !root.is_dir() {
            return Err(Error::Config(format!(
                "로컬 루트 {}가 디렉터리가 아닙니다",
                root.display()
            )));
        }
        Ok(Self { root })
    }

    /// `RemotePath`는 `..`를 거부하므로 결과는 항상 루트 하위입니다.
    fn local(&self, path: &RemotePath) -> PathBuf {
        let mut p = self.root.clone();
        p.extend(path.components());
        p
    }
}

fn entry_from(name: &str, meta: &std::fs::Metadata) -> Entry {
    let modified = meta.modified().ok();
    let size = if meta.is_dir() { 0 } else { meta.len() };
    let stamp = modified
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    Entry {
        name: name.to_owned(),
        kind: if meta.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        },
        size,
        modified,
        created: meta.created().ok(),
        etag: Some(format!("{stamp:x}-{size:x}")),
        readonly: meta.permissions().readonly(),
    }
}

async fn stat_local(path: &Path, name: &str) -> Result<Entry> {
    let meta = tokio::fs::metadata(path).await?;
    Ok(entry_from(name, &meta))
}

#[async_trait]
impl Backend for LocalBackend {
    fn kind(&self) -> &'static str {
        "local"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            set_mtime: true,
            ..Capabilities::default()
        }
    }

    async fn stat(&self, path: &RemotePath) -> Result<Entry> {
        stat_local(&self.local(path), path.name().unwrap_or("")).await
    }

    async fn list(&self, path: &RemotePath) -> Result<Vec<Entry>> {
        let mut rd = tokio::fs::read_dir(self.local(path)).await?;
        let mut out = Vec::new();
        while let Some(de) = rd.next_entry().await? {
            let Ok(name) = de.file_name().into_string() else {
                tracing::warn!(dir = %path, "UTF-8이 아닌 이름을 건너뜁니다");
                continue;
            };
            match de.metadata().await {
                Ok(meta) => out.push(entry_from(&name, &meta)),
                // 열거 중 삭제된 항목은 건너뜁니다.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(out)
    }

    async fn read_range(&self, path: &RemotePath, offset: u64, len: u64) -> Result<Bytes> {
        let mut f = tokio::fs::File::open(self.local(path)).await?;
        if f.metadata().await?.is_dir() {
            return Err(Error::IsADirectory);
        }
        f.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut buf = Vec::with_capacity(len.min(16 << 20) as usize);
        f.take(len).read_to_end(&mut buf).await?;
        Ok(Bytes::from(buf))
    }

    async fn write(&self, path: &RemotePath, source: WriteSource) -> Result<Entry> {
        let target = self.local(path);
        match source {
            WriteSource::Bytes(b) => tokio::fs::write(&target, &b).await?,
            WriteSource::File { path: src, .. } => {
                tokio::fs::copy(&src, &target).await?;
            }
        }
        stat_local(&target, path.name().unwrap_or("")).await
    }

    async fn mkdir(&self, path: &RemotePath) -> Result<Entry> {
        let target = self.local(path);
        tokio::fs::create_dir(&target).await?;
        stat_local(&target, path.name().unwrap_or("")).await
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> Result<()> {
        let (src, dst) = (self.local(from), self.local(to));
        // 대소문자만 바뀌는 이름 변경은 같은 항목이므로 존재 검사에서 제외합니다.
        let same = from.as_str().eq_ignore_ascii_case(to.as_str());
        if !same && tokio::fs::try_exists(&dst).await? {
            if !replace {
                return Err(Error::AlreadyExists);
            }
            if tokio::fs::metadata(&dst).await?.is_dir() {
                return Err(Error::IsADirectory);
            }
        }
        tokio::fs::rename(&src, &dst).await?;
        Ok(())
    }

    async fn delete(&self, path: &RemotePath, kind: EntryKind) -> Result<()> {
        let target = self.local(path);
        match kind {
            EntryKind::File => tokio::fs::remove_file(&target).await?,
            EntryKind::Dir => tokio::fs::remove_dir(&target).await?,
        }
        Ok(())
    }

    async fn set_mtime(&self, path: &RemotePath, mtime: SystemTime) -> Result<()> {
        let target = self.local(path);
        tokio::task::spawn_blocking(move || {
            let f = std::fs::OpenOptions::new().write(true).open(target)?;
            f.set_modified(mtime)
        })
        .await
        .map_err(|e| Error::Io(std::io::Error::other(e)))??;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RemotePath {
        RemotePath::parse(s).unwrap()
    }

    #[tokio::test]
    async fn crud_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let be = LocalBackend::new(dir.path()).unwrap();

        be.mkdir(&p("/d")).await.unwrap();
        let e = be
            .write(
                &p("/d/a.txt"),
                WriteSource::Bytes(Bytes::from_static(b"hello world")),
            )
            .await
            .unwrap();
        assert_eq!((e.name.as_str(), e.size), ("a.txt", 11));

        assert_eq!(
            &be.read_range(&p("/d/a.txt"), 6, 100).await.unwrap()[..],
            b"world"
        );
        assert!(
            be.read_range(&p("/d/a.txt"), 50, 10)
                .await
                .unwrap()
                .is_empty()
        );

        let names: Vec<_> = be
            .list(&p("/d"))
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["a.txt"]);

        be.rename(&p("/d/a.txt"), &p("/d/b.txt"), false)
            .await
            .unwrap();
        assert!(matches!(
            be.stat(&p("/d/a.txt")).await,
            Err(Error::NotFound)
        ));
        assert!(matches!(
            be.delete(&p("/d"), EntryKind::Dir).await,
            Err(Error::DirectoryNotEmpty)
        ));
        be.delete(&p("/d/b.txt"), EntryKind::File).await.unwrap();
        be.delete(&p("/d"), EntryKind::Dir).await.unwrap();
        assert!(be.list(&RemotePath::root()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rename_without_replace_keeps_target() {
        let dir = tempfile::tempdir().unwrap();
        let be = LocalBackend::new(dir.path()).unwrap();
        for (n, body) in [("/a", "A"), ("/b", "B")] {
            be.write(&p(n), WriteSource::Bytes(Bytes::from(body)))
                .await
                .unwrap();
        }
        assert!(matches!(
            be.rename(&p("/a"), &p("/b"), false).await,
            Err(Error::AlreadyExists)
        ));
        be.rename(&p("/a"), &p("/b"), true).await.unwrap();
        assert_eq!(&be.read_range(&p("/b"), 0, 10).await.unwrap()[..], b"A");
    }

    #[tokio::test]
    async fn write_from_file_and_set_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        std::fs::write(&src, vec![7u8; 1000]).unwrap();
        std::fs::create_dir(dir.path().join("root")).unwrap();
        let be = LocalBackend::new(dir.path().join("root")).unwrap();
        let e = be
            .write(
                &p("/f.bin"),
                WriteSource::File {
                    path: src,
                    size: 1000,
                },
            )
            .await
            .unwrap();
        assert_eq!(e.size, 1000);

        let t = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        be.set_mtime(&p("/f.bin"), t).await.unwrap();
        assert_eq!(be.stat(&p("/f.bin")).await.unwrap().modified, Some(t));
    }

    #[tokio::test]
    async fn list_on_file_is_not_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let be = LocalBackend::new(dir.path()).unwrap();
        be.write(&p("/f"), WriteSource::Bytes(Bytes::new()))
            .await
            .unwrap();
        assert!(matches!(be.list(&p("/f")).await, Err(Error::NotADirectory)));
    }
}
