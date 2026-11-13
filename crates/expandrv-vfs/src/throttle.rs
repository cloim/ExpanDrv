//! 토큰 버킷 기반 드라이브별 업로드와 다운로드 속도 제한.
//!
//! 백엔드 트레이트가 바이트 단위 스트림을 노출하지 않으므로 요청 단위로 대기합니다.
//! 다운로드는 받은 뒤, 업로드는 보내기 전에 크기만큼 토큰을 빌리고 부족분만큼 쉽니다.
//! 장기 평균 속도는 제한값을 넘지 않지만 큰 파일 하나는 몰아서 전송될 수 있습니다.

use async_trait::async_trait;
use bytes::Bytes;
use expandrv_core::{
    Backend, Capabilities, Entry, EntryKind, Quota, RemotePath, Result, UploadHint, UploadSession,
    WriteSource,
};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

pub(crate) struct TokenBucket {
    rate: f64,
    state: Mutex<(f64, Instant)>,
}

impl TokenBucket {
    /// 버스트 허용량은 1초 분량입니다.
    pub fn new(bytes_per_sec: u64) -> Self {
        let rate = bytes_per_sec as f64;
        Self {
            rate,
            state: Mutex::new((rate, Instant::now())),
        }
    }

    /// `n`바이트를 빌리고 기다려야 할 시간을 돌려줍니다.
    pub fn reserve(&self, n: u64, now: Instant) -> Duration {
        let mut st = self.state.lock();
        let (tokens, last) = *st;
        let refilled =
            (tokens + now.saturating_duration_since(last).as_secs_f64() * self.rate).min(self.rate);
        let left = refilled - n as f64;
        *st = (left, now);
        if left >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-left / self.rate)
        }
    }

    async fn take(&self, n: u64) {
        let wait = self.reserve(n, Instant::now());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

struct Throttled {
    inner: Arc<dyn Backend>,
    up: Option<Arc<TokenBucket>>,
    down: Option<Arc<TokenBucket>>,
}

/// 보내기 전에 토큰을 받아 스트리밍 업로드 속도를 제한합니다.
struct ThrottledUpload {
    inner: Box<dyn UploadSession>,
    up: Option<Arc<TokenBucket>>,
}

#[async_trait]
impl UploadSession for ThrottledUpload {
    async fn write(&mut self, offset: u64, data: Bytes) -> Result<()> {
        if let Some(b) = &self.up {
            b.take(data.len() as u64).await;
        }
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

/// 제한이 없으면 원래 백엔드를 그대로 돌려줍니다.
pub fn throttle(inner: Arc<dyn Backend>, up_bytes: u64, down_bytes: u64) -> Arc<dyn Backend> {
    if up_bytes == 0 && down_bytes == 0 {
        return inner;
    }
    Arc::new(Throttled {
        inner,
        up: (up_bytes > 0).then(|| Arc::new(TokenBucket::new(up_bytes))),
        down: (down_bytes > 0).then(|| Arc::new(TokenBucket::new(down_bytes))),
    })
}

#[async_trait]
impl Backend for Throttled {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    async fn stat(&self, path: &RemotePath) -> Result<Entry> {
        self.inner.stat(path).await
    }
    async fn list(&self, path: &RemotePath) -> Result<Vec<Entry>> {
        self.inner.list(path).await
    }
    async fn read_range(&self, path: &RemotePath, offset: u64, len: u64) -> Result<Bytes> {
        let data = self.inner.read_range(path, offset, len).await?;
        if let Some(b) = &self.down {
            b.take(data.len() as u64).await;
        }
        Ok(data)
    }
    async fn write(&self, path: &RemotePath, source: WriteSource) -> Result<Entry> {
        if let Some(b) = &self.up {
            b.take(source.len()).await;
        }
        self.inner.write(path, source).await
    }
    async fn begin_upload(
        &self,
        path: &RemotePath,
        hint: UploadHint,
    ) -> Result<Box<dyn UploadSession>> {
        let inner = self.inner.begin_upload(path, hint).await?;
        Ok(Box::new(ThrottledUpload {
            inner,
            up: self.up.clone(),
        }))
    }
    async fn mkdir(&self, path: &RemotePath) -> Result<Entry> {
        self.inner.mkdir(path).await
    }
    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> Result<()> {
        self.inner.rename(from, to, replace).await
    }
    async fn delete(&self, path: &RemotePath, kind: EntryKind) -> Result<()> {
        self.inner.delete(path, kind).await
    }
    async fn set_mtime(&self, path: &RemotePath, mtime: SystemTime) -> Result<()> {
        self.inner.set_mtime(path, mtime).await
    }
    async fn quota(&self) -> Result<Quota> {
        self.inner.quota().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_allows_one_second_burst_then_paces() {
        let b = TokenBucket::new(100_000);
        let t0 = Instant::now();
        assert_eq!(b.reserve(100_000, t0), Duration::ZERO);
        // 버킷이 비었으므로 50KB는 0.5초를 기다립니다.
        let w = b.reserve(50_000, t0);
        assert!((w.as_secs_f64() - 0.5).abs() < 1e-6, "{w:?}");
        // 1.5초 뒤에는 부채를 갚고 1초 분량까지만 다시 찹니다.
        assert_eq!(
            b.reserve(100_000, t0 + Duration::from_millis(2000)),
            Duration::ZERO
        );
    }

    #[test]
    fn large_request_waits_proportionally() {
        let b = TokenBucket::new(1_000);
        let w = b.reserve(5_000, Instant::now());
        assert!((w.as_secs_f64() - 4.0).abs() < 0.01, "{w:?}");
    }
}
