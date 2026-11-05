//! 암호화 디스크 블록 캐시. 한 번 읽은 블록을 재시작 뒤와 오프라인에서도 다시 쓰게 합니다.
//!
//! - 파일 이름: 드라이브 키로 만든 HMAC-SHA256(경로, 버전, 순번). 이름으로 원격 경로를 알 수 없습니다.
//! - 본문: AES-256-GCM, 파일 이름을 추가 인증 데이터로 묶어 다른 블록으로 바꿔치기할 수 없습니다.
//! - 색인: 같은 폴더의 SQLite에 크기와 마지막 사용 시각을 두고 상한을 넘으면 오래된 것부터 지웁니다.

use bytes::Bytes;
use expandrv_core::{Error, RemotePath, Result};
use parking_lot::Mutex;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{Connection, params};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const FORMAT: u8 = 1;
/// 상한을 넘으면 이 비율까지 줄입니다.
const EVICT_TARGET: f64 = 0.9;

/// 디스크 캐시 설정. 키는 드라이브마다 만들어 자격 증명 관리자 등 안전한 곳에 둡니다.
#[derive(Clone)]
pub struct DiskCacheOptions {
    pub dir: PathBuf,
    pub max_bytes: u64,
    pub key: [u8; 32],
}

impl std::fmt::Debug for DiskCacheOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskCacheOptions")
            .field("dir", &self.dir)
            .field("max_bytes", &self.max_bytes)
            .finish_non_exhaustive()
    }
}

/// 새 캐시 키(32바이트 난수)
pub fn generate_key() -> Result<[u8; 32]> {
    let mut k = [0u8; 32];
    SystemRandom::new()
        .fill(&mut k)
        .map_err(|_| Error::Io(std::io::Error::other("난수 생성 실패")))?;
    Ok(k)
}

pub(crate) struct DiskCache {
    dir: PathBuf,
    max_bytes: u64,
    aead: LessSafeKey,
    names: hmac::Key,
    rng: SystemRandom,
    index: Mutex<(Connection, u64)>,
}

fn db_err(e: rusqlite::Error) -> Error {
    Error::Io(std::io::Error::other(e))
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as i64)
}

/// 용도별 하위 키: HMAC(주 키, 레이블)
fn derive(key: &[u8; 32], label: &[u8]) -> [u8; 32] {
    let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), label);
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}

impl DiskCache {
    pub fn open(opts: &DiskCacheOptions) -> Result<Self> {
        std::fs::create_dir_all(&opts.dir)?;
        let conn = Connection::open(opts.dir.join("index.db")).map_err(db_err)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS blocks(
                 name TEXT PRIMARY KEY,
                 path TEXT NOT NULL,
                 size INTEGER NOT NULL,
                 used_ns INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS blocks_used ON blocks(used_ns);
             CREATE INDEX IF NOT EXISTS blocks_path ON blocks(path);",
        )
        .map_err(db_err)?;
        let total: i64 = conn
            .query_row("SELECT COALESCE(SUM(size), 0) FROM blocks", [], |r| {
                r.get(0)
            })
            .map_err(db_err)?;
        let aead_key = derive(&opts.key, b"expandrv-cache-aead-v1");
        Ok(Self {
            dir: opts.dir.clone(),
            max_bytes: opts.max_bytes,
            aead: LessSafeKey::new(
                UnboundKey::new(&AES_256_GCM, &aead_key)
                    .map_err(|_| Error::Config("캐시 키가 올바르지 않습니다".into()))?,
            ),
            names: hmac::Key::new(
                hmac::HMAC_SHA256,
                &derive(&opts.key, b"expandrv-cache-name-v1"),
            ),
            rng: SystemRandom::new(),
            index: Mutex::new((conn, total.max(0) as u64)),
        })
    }

    fn name(&self, path: &RemotePath, version: &str, idx: u64) -> String {
        let mut ctx = hmac::Context::with_key(&self.names);
        ctx.update(path.as_str().as_bytes());
        ctx.update(&[0]);
        ctx.update(version.as_bytes());
        ctx.update(&[0]);
        ctx.update(&idx.to_le_bytes());
        ctx.sign()
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(&name[..2]).join(name)
    }

    pub fn get(&self, path: &RemotePath, version: &str, idx: u64) -> Option<Bytes> {
        let name = self.name(path, version, idx);
        let mut data = std::fs::read(self.file(&name)).ok()?;
        let plain_len = (|| {
            if data.len() < 1 + NONCE_LEN || data[0] != FORMAT {
                return None;
            }
            let nonce = Nonce::try_assume_unique_for_key(&data[1..1 + NONCE_LEN]).ok()?;
            let body = &mut data[1 + NONCE_LEN..];
            let plain = self
                .aead
                .open_in_place(nonce, Aad::from(name.as_bytes()), body)
                .ok()?;
            Some(plain.len())
        })();
        let Some(len) = plain_len else {
            // 다른 키로 만들었거나 손상된 블록은 지웁니다.
            tracing::debug!(%path, idx, "디스크 캐시 블록을 해독하지 못해 지웁니다");
            self.remove_names(&[name]);
            return None;
        };
        let _ = self.index.lock().0.execute(
            "UPDATE blocks SET used_ns = ?2 WHERE name = ?1",
            params![name, now_ns()],
        );
        data.drain(..1 + NONCE_LEN);
        data.truncate(len);
        Some(Bytes::from(data))
    }

    pub fn put(&self, path: &RemotePath, version: &str, idx: u64, data: &[u8]) -> Result<()> {
        let name = self.name(path, version, idx);
        let mut nonce = [0u8; NONCE_LEN];
        self.rng
            .fill(&mut nonce)
            .map_err(|_| Error::Io(std::io::Error::other("난수 생성 실패")))?;
        let mut buf = Vec::with_capacity(1 + NONCE_LEN + data.len() + AES_256_GCM.tag_len());
        buf.push(FORMAT);
        buf.extend_from_slice(&nonce);
        let start = buf.len();
        buf.extend_from_slice(data);
        let tag = self
            .aead
            .seal_in_place_separate_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(name.as_bytes()),
                &mut buf[start..],
            )
            .map_err(|_| Error::Io(std::io::Error::other("캐시 암호화 실패")))?;
        buf.extend_from_slice(tag.as_ref());

        let file = self.file(&name);
        std::fs::create_dir_all(file.parent().expect("shard"))?;
        let tmp = file.with_extension("tmp");
        std::fs::write(&tmp, &buf)?;
        std::fs::rename(&tmp, &file)?;

        let size = buf.len() as u64;
        let over = {
            let mut idx_db = self.index.lock();
            let old: Option<i64> = idx_db
                .0
                .query_row("SELECT size FROM blocks WHERE name = ?1", [&name], |r| {
                    r.get(0)
                })
                .ok();
            idx_db
                .0
                .execute(
                    "INSERT OR REPLACE INTO blocks(name, path, size, used_ns) VALUES (?1, ?2, ?3, ?4)",
                    params![name, path.as_str(), size as i64, now_ns()],
                )
                .map_err(db_err)?;
            idx_db.1 = idx_db.1 + size - old.unwrap_or(0).max(0) as u64;
            idx_db.1 > self.max_bytes
        };
        if over {
            self.evict();
        }
        Ok(())
    }

    /// 상한의 90%까지 오래 쓰지 않은 블록부터 지웁니다.
    fn evict(&self) {
        let target = (self.max_bytes as f64 * EVICT_TARGET) as u64;
        let victims: Vec<String> = {
            let idx_db = self.index.lock();
            let mut excess = idx_db.1.saturating_sub(target) as i64;
            let Ok(mut q) = idx_db
                .0
                .prepare("SELECT name, size FROM blocks ORDER BY used_ns")
            else {
                return;
            };
            let Ok(rows) = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            else {
                return;
            };
            let mut out = Vec::new();
            for (name, size) in rows.flatten() {
                if excess <= 0 {
                    break;
                }
                excess -= size;
                out.push(name);
            }
            out
        };
        self.remove_names(&victims);
    }

    fn remove_names(&self, names: &[String]) {
        let mut idx_db = self.index.lock();
        for name in names {
            let _ = std::fs::remove_file(self.file(name));
            let size: Option<i64> = idx_db
                .0
                .query_row("SELECT size FROM blocks WHERE name = ?1", [name], |r| {
                    r.get(0)
                })
                .ok();
            let _ = idx_db
                .0
                .execute("DELETE FROM blocks WHERE name = ?1", [name]);
            idx_db.1 = idx_db.1.saturating_sub(size.unwrap_or(0).max(0) as u64);
        }
    }

    /// 경로와 하위 경로의 블록을 모두 지웁니다(업로드, 이름 변경, 삭제, 원격 변경 뒤).
    pub fn invalidate(&self, path: &RemotePath) {
        let names: Vec<String> = {
            let idx_db = self.index.lock();
            let like = if path.is_root() {
                "/%".to_owned()
            } else {
                format!(
                    "{}/%",
                    path.as_str().replace('%', "\\%").replace('_', "\\_")
                )
            };
            let Ok(mut q) = idx_db
                .0
                .prepare("SELECT name FROM blocks WHERE path = ?1 OR path LIKE ?2 ESCAPE '\\'")
            else {
                return;
            };
            q.query_map(params![path.as_str(), like], |r| r.get::<_, String>(0))
                .map(|rows| rows.flatten().collect())
                .unwrap_or_default()
        };
        self.remove_names(&names);
    }

    #[cfg(test)]
    pub fn used_bytes(&self) -> u64 {
        self.index.lock().1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn p(s: &str) -> RemotePath {
        RemotePath::parse(s).unwrap()
    }

    fn open(dir: &Path, max: u64, key: [u8; 32]) -> DiskCache {
        DiskCache::open(&DiskCacheOptions {
            dir: dir.to_owned(),
            max_bytes: max,
            key,
        })
        .unwrap()
    }

    fn all_files(dir: &Path) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for shard in std::fs::read_dir(dir).unwrap().flatten() {
            if shard.path().is_dir() {
                for f in std::fs::read_dir(shard.path()).unwrap().flatten() {
                    out.push(std::fs::read(f.path()).unwrap());
                }
            }
        }
        out
    }

    #[test]
    fn roundtrip_persists_and_is_encrypted() {
        let dir = tempfile::tempdir().unwrap();
        let key = generate_key().unwrap();
        let secret = b"top secret block contents 0123456789".repeat(100);
        {
            let c = open(dir.path(), 1 << 20, key);
            c.put(&p("/docs/plan.txt"), "v1", 0, &secret).unwrap();
            assert_eq!(c.get(&p("/docs/plan.txt"), "v1", 0).unwrap(), secret);
            assert!(
                c.get(&p("/docs/plan.txt"), "v2", 0).is_none(),
                "버전이 다르면 없음"
            );
        }
        // 다시 열어도 남아 있고, 파일 내용과 이름에 평문이 없습니다.
        let c = open(dir.path(), 1 << 20, key);
        assert_eq!(c.get(&p("/docs/plan.txt"), "v1", 0).unwrap(), secret);
        let files = all_files(dir.path());
        assert_eq!(files.len(), 1);
        assert!(!files[0].windows(10).any(|w| w == &secret[..10]));
        for e in walk_names(dir.path()) {
            assert!(!e.contains("plan"), "{e}");
        }
    }

    fn walk_names(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        for shard in std::fs::read_dir(dir).unwrap().flatten() {
            if shard.path().is_dir() {
                for f in std::fs::read_dir(shard.path()).unwrap().flatten() {
                    out.push(f.file_name().into_string().unwrap());
                }
            }
        }
        out
    }

    #[test]
    fn wrong_key_or_swapped_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let c = open(dir.path(), 1 << 20, generate_key().unwrap());
        c.put(&p("/a"), "v", 0, b"AAAA").unwrap();
        c.put(&p("/b"), "v", 0, b"BBBB").unwrap();
        // 블록 파일을 서로 바꿔치기하면 인증에 실패합니다.
        let (fa, fb) = (
            c.file(&c.name(&p("/a"), "v", 0)),
            c.file(&c.name(&p("/b"), "v", 0)),
        );
        let (da, db) = (std::fs::read(&fa).unwrap(), std::fs::read(&fb).unwrap());
        std::fs::write(&fa, db).unwrap();
        std::fs::write(&fb, da).unwrap();
        assert!(c.get(&p("/a"), "v", 0).is_none());
        assert!(!fa.exists(), "손상 블록은 지웁니다");

        let other = open(dir.path(), 1 << 20, generate_key().unwrap());
        assert!(other.get(&p("/b"), "v", 0).is_none());
    }

    #[test]
    fn evicts_least_recently_used_and_invalidates_subtree() {
        let dir = tempfile::tempdir().unwrap();
        let c = open(dir.path(), 3_500, generate_key().unwrap());
        let block = vec![7u8; 900];
        c.put(&p("/d/1"), "v", 0, &block).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        c.put(&p("/d/2"), "v", 0, &block).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(c.get(&p("/d/1"), "v", 0).is_some()); // 1을 최근 사용으로
        std::thread::sleep(std::time::Duration::from_millis(5));
        c.put(&p("/e/3"), "v", 0, &block).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        c.put(&p("/e/4"), "v", 0, &block).unwrap();
        assert!(c.used_bytes() <= 3_500);
        assert!(c.get(&p("/d/2"), "v", 0).is_none(), "가장 오래된 블록 제거");
        assert!(c.get(&p("/e/4"), "v", 0).is_some());

        c.invalidate(&p("/e"));
        assert!(c.get(&p("/e/4"), "v", 0).is_none());
        assert!(c.get(&p("/d/1"), "v", 0).is_some());
    }
}
