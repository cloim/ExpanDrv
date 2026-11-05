//! 메모리 블록 캐시. 원격 파일을 고정 크기 블록으로 받아 LRU로 보관합니다.
//!
//! 블록은 항목 버전(ETag 또는 크기와 수정 시각)과 함께 저장하므로 원격 파일이 바뀌면 자동으로 무효가 됩니다.

use bytes::Bytes;
use expandrv_core::RemotePath;
use lru::LruCache;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::OnceCell;

type Key = (RemotePath, u64);

pub(crate) struct BlockCache {
    block_size: u64,
    capacity: usize,
    inner: Mutex<Inner>,
}

struct Inner {
    lru: LruCache<Key, (Arc<str>, Bytes)>,
    bytes: usize,
    /// 진행 중인 내려받기. 같은 블록을 동시에 요청하면 한 번만 받습니다.
    inflight: HashMap<Key, Arc<OnceCell<Option<Bytes>>>>,
}

impl BlockCache {
    pub fn new(block_size: u64, capacity: usize) -> Self {
        Self {
            block_size: block_size.max(64 * 1024),
            capacity,
            inner: Mutex::new(Inner {
                lru: LruCache::unbounded(),
                bytes: 0,
                inflight: HashMap::new(),
            }),
        }
    }

    pub fn block_size(&self) -> u64 {
        self.block_size
    }

    pub fn get(&self, path: &RemotePath, version: &str, idx: u64) -> Option<Bytes> {
        let mut inner = self.inner.lock();
        let key = (path.clone(), idx);
        match inner.lru.get(&key) {
            Some((v, data)) if &**v == version => Some(data.clone()),
            Some(_) => {
                if let Some((_, old)) = inner.lru.pop(&key) {
                    inner.bytes -= old.len();
                }
                None
            }
            None => None,
        }
    }

    pub fn put(&self, path: &RemotePath, version: &str, idx: u64, data: Bytes) {
        if data.len() > self.capacity {
            return;
        }
        let mut inner = self.inner.lock();
        inner.bytes += data.len();
        if let Some((_, old)) = inner
            .lru
            .put((path.clone(), idx), (Arc::from(version), data))
        {
            inner.bytes -= old.len();
        }
        while inner.bytes > self.capacity {
            match inner.lru.pop_lru() {
                Some((_, (_, old))) => inner.bytes -= old.len(),
                None => break,
            }
        }
    }

    /// 진행 중 내려받기 셀을 얻습니다. 새로 만든 경우 `true`를 함께 돌려줍니다.
    pub fn inflight(&self, path: &RemotePath, idx: u64) -> (Arc<OnceCell<Option<Bytes>>>, bool) {
        let mut inner = self.inner.lock();
        let key = (path.clone(), idx);
        if let Some(cell) = inner.inflight.get(&key) {
            return (cell.clone(), false);
        }
        let cell = Arc::new(OnceCell::new());
        inner.inflight.insert(key, cell.clone());
        (cell, true)
    }

    pub fn finish_inflight(&self, path: &RemotePath, idx: u64) {
        self.inner.lock().inflight.remove(&(path.clone(), idx));
    }

    pub fn invalidate(&self, path: &RemotePath) {
        let mut inner = self.inner.lock();
        let keys: Vec<Key> = inner
            .lru
            .iter()
            .filter(|((p, _), _)| p.starts_with(path))
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            if let Some((_, old)) = inner.lru.pop(&k) {
                inner.bytes -= old.len();
            }
        }
    }

    #[cfg(test)]
    pub fn used_bytes(&self) -> usize {
        self.inner.lock().bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RemotePath {
        RemotePath::parse(s).unwrap()
    }

    #[test]
    fn evicts_to_capacity_and_checks_version() {
        let c = BlockCache::new(64 * 1024, 100);
        c.put(&p("/a"), "v1", 0, Bytes::from(vec![0; 60]));
        c.put(&p("/a"), "v1", 1, Bytes::from(vec![1; 60]));
        assert!(c.get(&p("/a"), "v1", 0).is_none(), "LRU 블록 제거");
        assert_eq!(c.get(&p("/a"), "v1", 1).unwrap()[0], 1);
        assert!(c.get(&p("/a"), "v2", 1).is_none(), "버전 불일치");
        assert_eq!(c.used_bytes(), 0);
    }

    #[test]
    fn invalidate_subtree() {
        let c = BlockCache::new(64 * 1024, 1000);
        c.put(&p("/d/x"), "v", 0, Bytes::from_static(b"x"));
        c.put(&p("/e"), "v", 0, Bytes::from_static(b"e"));
        c.invalidate(&p("/d"));
        assert!(c.get(&p("/d/x"), "v", 0).is_none());
        assert!(c.get(&p("/e"), "v", 0).is_some());
        assert_eq!(c.used_bytes(), 1);
    }
}
