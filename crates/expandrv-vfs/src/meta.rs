//! 메모리 메타 캐시: 항목 속성과 디렉터리 목록을 TTL 동안 보관합니다.

use expandrv_core::{Entry, RemotePath};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) struct MetaCache {
    ttl: Duration,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<RemotePath, (Entry, Instant)>,
    dirs: HashMap<RemotePath, (Arc<Vec<Entry>>, Instant)>,
}

/// 부모 목록 조회 결과
pub(crate) enum Lookup {
    Found(RemotePath, Entry),
    Missing,
    Unknown,
}

impl MetaCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(Inner::default()),
        }
    }

    fn fresh(&self, at: Instant) -> bool {
        at.elapsed() < self.ttl
    }

    pub fn entry(&self, path: &RemotePath) -> Option<Entry> {
        let inner = self.inner.lock();
        inner
            .entries
            .get(path)
            .filter(|(_, at)| self.fresh(*at))
            .map(|(e, _)| e.clone())
    }

    pub fn dir(&self, path: &RemotePath) -> Option<Arc<Vec<Entry>>> {
        let inner = self.inner.lock();
        inner
            .dirs
            .get(path)
            .filter(|(_, at)| self.fresh(*at))
            .map(|(d, _)| d.clone())
    }

    /// 신선한 부모 목록에서 이름을 찾습니다. 정확한 이름을 먼저, 없으면 대소문자 무시로 찾습니다.
    pub fn lookup(&self, path: &RemotePath) -> Lookup {
        let (Some(parent), Some(name)) = (path.parent(), path.name()) else {
            return Lookup::Unknown;
        };
        let Some(list) = self.dir(&parent) else {
            return Lookup::Unknown;
        };
        let hit = list
            .iter()
            .find(|e| e.name == name)
            .or_else(|| list.iter().find(|e| e.name.eq_ignore_ascii_case(name)))
            .or_else(|| {
                list.iter()
                    .find(|e| e.name.to_lowercase() == name.to_lowercase())
            });
        match hit {
            Some(e) => match parent.join(&e.name) {
                Ok(p) => Lookup::Found(p, e.clone()),
                Err(_) => Lookup::Unknown,
            },
            None => Lookup::Missing,
        }
    }

    pub fn put_entry(&self, path: &RemotePath, entry: Entry) {
        self.inner
            .lock()
            .entries
            .insert(path.clone(), (entry, Instant::now()));
    }

    pub fn put_dir(&self, path: &RemotePath, list: Arc<Vec<Entry>>) {
        let now = Instant::now();
        let mut inner = self.inner.lock();
        for e in list.iter() {
            if let Ok(child) = path.join(&e.name) {
                inner.entries.insert(child, (e.clone(), now));
            }
        }
        inner.dirs.insert(path.clone(), (list, now));
    }

    /// TTL과 무관하게 마지막으로 받은 목록. 원격 변경 비교 기준으로 씁니다.
    pub fn snapshot(&self, path: &RemotePath) -> Option<Arc<Vec<Entry>>> {
        self.inner.lock().dirs.get(path).map(|(d, _)| d.clone())
    }

    /// 목록이 `expected` 그대로일 때만 새 목록으로 바꿉니다.
    /// 폴링 도중 로컬 변경으로 무효화된 목록을 오래된 원격 결과로 덮지 않기 위함입니다.
    pub fn replace_dir_if(
        &self,
        path: &RemotePath,
        expected: &Arc<Vec<Entry>>,
        list: Arc<Vec<Entry>>,
    ) -> bool {
        let mut inner = self.inner.lock();
        if !inner
            .dirs
            .get(path)
            .is_some_and(|(d, _)| Arc::ptr_eq(d, expected))
        {
            return false;
        }
        // 사라진 항목의 속성 캐시도 함께 지웁니다.
        for e in expected.iter() {
            if let Ok(child) = path.join(&e.name) {
                inner.entries.remove(&child);
            }
        }
        let now = Instant::now();
        for e in list.iter() {
            if let Ok(child) = path.join(&e.name) {
                inner.entries.insert(child, (e.clone(), now));
            }
        }
        inner.dirs.insert(path.clone(), (list, now));
        true
    }

    /// 항목과 부모 목록을 무효화합니다. 생성, 삭제, 크기 변경 후 호출합니다.
    pub fn invalidate(&self, path: &RemotePath) {
        let mut inner = self.inner.lock();
        inner.entries.remove(path);
        if let Some(parent) = path.parent() {
            inner.dirs.remove(&parent);
        }
    }

    /// 하위 트리 전체를 무효화합니다. 이름 변경과 디렉터리 삭제 후 호출합니다.
    pub fn invalidate_tree(&self, path: &RemotePath) {
        let mut inner = self.inner.lock();
        inner.entries.retain(|p, _| !p.starts_with(path));
        inner.dirs.retain(|p, _| !p.starts_with(path));
        if let Some(parent) = path.parent() {
            inner.dirs.remove(&parent);
            inner.entries.remove(path);
        }
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock();
        inner.entries.clear();
        inner.dirs.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RemotePath {
        RemotePath::parse(s).unwrap()
    }

    #[test]
    fn lookup_uses_parent_listing_case_insensitively() {
        let c = MetaCache::new(Duration::from_secs(60));
        c.put_dir(&p("/d"), Arc::new(vec![Entry::file("Readme.TXT", 3)]));
        match c.lookup(&p("/d/readme.txt")) {
            Lookup::Found(path, e) => assert_eq!((path.as_str(), e.size), ("/d/Readme.TXT", 3)),
            _ => panic!("expected hit"),
        }
        assert!(matches!(c.lookup(&p("/d/desktop.ini")), Lookup::Missing));
        assert!(matches!(c.lookup(&p("/other/x")), Lookup::Unknown));
    }

    #[test]
    fn invalidate_tree_drops_descendants() {
        let c = MetaCache::new(Duration::from_secs(60));
        c.put_dir(&p("/"), Arc::new(vec![Entry::dir("a")]));
        c.put_dir(&p("/a"), Arc::new(vec![Entry::file("f", 1)]));
        c.invalidate_tree(&p("/a"));
        assert!(c.dir(&p("/a")).is_none());
        assert!(c.entry(&p("/a/f")).is_none());
        assert!(c.dir(&p("/")).is_none());
    }

    #[test]
    fn entries_expire() {
        let c = MetaCache::new(Duration::ZERO);
        c.put_entry(&p("/x"), Entry::file("x", 1));
        assert!(c.entry(&p("/x")).is_none());
    }
}
