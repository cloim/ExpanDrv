//! 백그라운드 원격 작업을 두 개로 제한해 사용자 요청에 연결 슬롯을 남깁니다.
use async_trait::async_trait;
use bytes::Bytes;
use expandrv_core::{
    Backend, Capabilities, Entry, EntryKind, Error, Quota, RemotePath, Result, UploadHint,
    UploadSession, WriteSource,
};
use std::{sync::Arc, time::SystemTime};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

struct Budget {
    all: Arc<Semaphore>,
    background: Arc<Semaphore>,
}

pub(crate) struct Scheduled {
    inner: Arc<dyn Backend>,
    budget: Arc<Budget>,
    background: bool,
    speculative: bool,
}

impl Scheduled {
    async fn enter(&self) -> Result<(Option<OwnedSemaphorePermit>, OwnedSemaphorePermit)> {
        let bg = if self.background {
            Some(if self.speculative {
                self.budget
                    .background
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| Error::Cancelled)?
            } else {
                self.budget
                    .background
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| Error::Cancelled)?
            })
        } else {
            None
        };
        let all = if self.speculative {
            self.budget
                .all
                .clone()
                .try_acquire_owned()
                .map_err(|_| Error::Cancelled)?
        } else {
            self.budget
                .all
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| Error::Cancelled)?
        };
        Ok((bg, all))
    }
}

/// 세션이 열려 있는 동안 연결 슬롯을 쥡니다.
struct ScheduledUpload {
    inner: Box<dyn UploadSession>,
    _permit: (Option<OwnedSemaphorePermit>, OwnedSemaphorePermit),
}

#[async_trait]
impl UploadSession for ScheduledUpload {
    async fn write(&mut self, offset: u64, data: Bytes) -> Result<()> {
        self.inner.write(offset, data).await
    }
    async fn flush(&mut self) -> Result<()> {
        self.inner.flush().await
    }
    async fn finish(self: Box<Self>) -> Result<Entry> {
        self.inner.finish().await
    }
    async fn abort(self: Box<Self>) {
        self.inner.abort().await;
    }
}

pub(crate) fn schedule(inner: Arc<dyn Backend>) -> [Arc<dyn Backend>; 3] {
    let budget = Arc::new(Budget {
        all: Arc::new(Semaphore::new(4)),
        background: Arc::new(Semaphore::new(2)),
    });
    [(false, false), (true, false), (true, true)].map(|(background, speculative)| {
        Arc::new(Scheduled {
            inner: inner.clone(),
            budget: budget.clone(),
            background,
            speculative,
        }) as Arc<dyn Backend>
    })
}

#[async_trait]
impl Backend for Scheduled {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    async fn stat(&self, path: &RemotePath) -> Result<Entry> {
        let _permit = self.enter().await?;
        self.inner.stat(path).await
    }
    async fn list(&self, path: &RemotePath) -> Result<Vec<Entry>> {
        let _permit = self.enter().await?;
        self.inner.list(path).await
    }
    async fn read_range(&self, path: &RemotePath, offset: u64, len: u64) -> Result<Bytes> {
        let _permit = self.enter().await?;
        self.inner.read_range(path, offset, len).await
    }
    async fn write(&self, path: &RemotePath, source: WriteSource) -> Result<Entry> {
        let _permit = self.enter().await?;
        self.inner.write(path, source).await
    }
    async fn begin_upload(
        &self,
        path: &RemotePath,
        hint: UploadHint,
    ) -> Result<Box<dyn UploadSession>> {
        let permit = self.enter().await?;
        let inner = self.inner.begin_upload(path, hint).await?;
        Ok(Box::new(ScheduledUpload {
            inner,
            _permit: permit,
        }))
    }
    async fn mkdir(&self, path: &RemotePath) -> Result<Entry> {
        let _permit = self.enter().await?;
        self.inner.mkdir(path).await
    }
    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> Result<()> {
        let _permit = self.enter().await?;
        self.inner.rename(from, to, replace).await
    }
    async fn delete(&self, path: &RemotePath, kind: EntryKind) -> Result<()> {
        let _permit = self.enter().await?;
        self.inner.delete(path, kind).await
    }
    async fn set_mtime(&self, path: &RemotePath, mtime: SystemTime) -> Result<()> {
        let _permit = self.enter().await?;
        self.inner.set_mtime(path, mtime).await
    }
    async fn quota(&self) -> Result<Quota> {
        let _permit = self.enter().await?;
        self.inner.quota().await
    }
}
