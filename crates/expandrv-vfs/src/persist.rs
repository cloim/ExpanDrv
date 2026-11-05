//! SQLite 메타 저장소. 마지막으로 받은 디렉터리 목록을 디스크에 보관해
//! 원격이 응답하지 않을 때 탐색을 이어갈 수 있게 합니다(오프라인 읽기 전용 폴백).

use expandrv_core::{Entry, EntryKind, RemotePath, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, params};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) struct MetaStore {
    conn: Mutex<Connection>,
}

fn db_err(e: rusqlite::Error) -> expandrv_core::Error {
    expandrv_core::Error::Io(std::io::Error::other(e))
}

fn to_ns(t: Option<SystemTime>) -> Option<i64> {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
}

fn from_ns(v: Option<i64>) -> Option<SystemTime> {
    v.map(|n| UNIX_EPOCH + Duration::from_nanos(n.max(0) as u64))
}

impl MetaStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).map_err(db_err)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS dirs(
                 path TEXT PRIMARY KEY,
                 fetched_ns INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS entries(
                 dir TEXT NOT NULL,
                 name TEXT NOT NULL,
                 is_dir INTEGER NOT NULL,
                 size INTEGER NOT NULL,
                 modified_ns INTEGER,
                 created_ns INTEGER,
                 etag TEXT,
                 readonly INTEGER NOT NULL,
                 PRIMARY KEY(dir, name));
             CREATE TABLE IF NOT EXISTS pending(
                 path TEXT PRIMARY KEY,
                 staging TEXT NOT NULL,
                 base_etag TEXT,
                 created_ns INTEGER NOT NULL);",
        )
        .map_err(db_err)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 업로드 저널: 아직 원격에 반영하지 않은 파일과 그 스테이징 파일, 편집 시작 때의 원격 버전
    pub fn journal_put(
        &self,
        path: &RemotePath,
        staging: &Path,
        base_etag: Option<&str>,
    ) -> Result<()> {
        self.conn
            .lock()
            .execute(
                "INSERT OR REPLACE INTO pending(path, staging, base_etag, created_ns) VALUES (?1, ?2, ?3, ?4)",
                params![
                    path.as_str(),
                    staging.to_string_lossy(),
                    base_etag,
                    to_ns(Some(SystemTime::now()))
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn journal_remove(&self, path: &RemotePath) -> Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM pending WHERE path = ?1", [path.as_str()])
            .map_err(db_err)?;
        Ok(())
    }

    /// 이름 변경으로 옮겨진 하위 트리의 저널 경로를 바꿉니다.
    pub fn journal_move(&self, from: &RemotePath, to: &RemotePath) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction().map_err(db_err)?;
        let moved: Vec<(String, RemotePath)> = {
            let mut q = tx.prepare("SELECT path FROM pending").map_err(db_err)?;
            let rows = q
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(db_err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(db_err)?;
            rows.into_iter()
                .filter_map(|p| {
                    let new = RemotePath::parse(&p).ok()?.rebase(from, to)?;
                    Some((p, new))
                })
                .collect()
        };
        for (old, new) in moved {
            tx.execute(
                "UPDATE OR REPLACE pending SET path = ?2 WHERE path = ?1",
                params![old, new.as_str()],
            )
            .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)
    }

    /// (원격 경로, 스테이징 파일, 원격 기준 버전)
    pub fn journal_all(&self) -> Result<Vec<(RemotePath, std::path::PathBuf, Option<String>)>> {
        let conn = self.conn.lock();
        let mut q = conn
            .prepare("SELECT path, staging, base_etag FROM pending ORDER BY created_ns")
            .map_err(db_err)?;
        let rows = q
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(db_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .filter_map(|(p, s, e)| Some((RemotePath::parse(&p).ok()?, s.into(), e)))
            .collect())
    }

    pub fn save_dir(&self, dir: &RemotePath, list: &[Entry]) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction().map_err(db_err)?;
        tx.execute("DELETE FROM entries WHERE dir = ?1", [dir.as_str()])
            .map_err(db_err)?;
        {
            let mut ins = tx
                .prepare_cached(
                    "INSERT INTO entries(dir, name, is_dir, size, modified_ns, created_ns, etag, readonly)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )
                .map_err(db_err)?;
            for e in list {
                ins.execute(params![
                    dir.as_str(),
                    e.name,
                    e.is_dir(),
                    e.size as i64,
                    to_ns(e.modified),
                    to_ns(e.created),
                    e.etag,
                    e.readonly
                ])
                .map_err(db_err)?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO dirs(path, fetched_ns) VALUES (?1, ?2)",
            params![dir.as_str(), to_ns(Some(SystemTime::now()))],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)
    }

    pub fn load_dir(&self, dir: &RemotePath) -> Result<Option<Vec<Entry>>> {
        let conn = self.conn.lock();
        let known: bool = conn
            .query_row("SELECT 1 FROM dirs WHERE path = ?1", [dir.as_str()], |_| {
                Ok(())
            })
            .map(|_| true)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(false),
                e => Err(e),
            })
            .map_err(db_err)?;
        if !known {
            return Ok(None);
        }
        let mut q = conn
            .prepare_cached(
                "SELECT name, is_dir, size, modified_ns, created_ns, etag, readonly
                 FROM entries WHERE dir = ?1 ORDER BY name",
            )
            .map_err(db_err)?;
        let rows = q
            .query_map([dir.as_str()], |r| {
                Ok(Entry {
                    name: r.get(0)?,
                    kind: if r.get::<_, bool>(1)? {
                        EntryKind::Dir
                    } else {
                        EntryKind::File
                    },
                    size: r.get::<_, i64>(2)? as u64,
                    modified: from_ns(r.get(3)?),
                    created: from_ns(r.get(4)?),
                    etag: r.get(5)?,
                    readonly: r.get(6)?,
                })
            })
            .map_err(db_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map(Some)
            .map_err(db_err)
    }

    /// 하위 트리 기록을 지웁니다. 이름 변경과 삭제 뒤 오래된 목록이 폴백되지 않게 합니다.
    pub fn forget_tree(&self, path: &RemotePath) -> Result<()> {
        let conn = self.conn.lock();
        let like = if path.is_root() {
            "/%".to_owned()
        } else {
            format!(
                "{}/%",
                path.as_str().replace('%', "\\%").replace('_', "\\_")
            )
        };
        for table_col in [("entries", "dir"), ("dirs", "path")] {
            conn.execute(
                &format!(
                    "DELETE FROM {0} WHERE {1} = ?1 OR {1} LIKE ?2 ESCAPE '\\'",
                    table_col.0, table_col.1
                ),
                params![path.as_str(), like],
            )
            .map_err(db_err)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_forget() {
        let dir = tempfile::tempdir().unwrap();
        let store = MetaStore::open(&dir.path().join("m/meta.db")).unwrap();
        let root = RemotePath::root();
        let sub = RemotePath::parse("/a_b").unwrap();
        let mut f = Entry::file("한글 파일.txt", 42);
        f.modified = Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        f.etag = Some("e1".into());
        store
            .save_dir(&root, &[Entry::dir("a_b"), f.clone()])
            .unwrap();
        store.save_dir(&sub, &[Entry::file("x", 1)]).unwrap();

        let got = store.load_dir(&root).unwrap().unwrap();
        assert_eq!(got.len(), 2);
        assert!(got.contains(&f));
        assert!(
            store
                .load_dir(&RemotePath::parse("/none").unwrap())
                .unwrap()
                .is_none()
        );

        // `_`는 LIKE 와일드카드로 해석되지 않아야 합니다.
        store
            .save_dir(&RemotePath::parse("/aXb").unwrap(), &[])
            .unwrap();
        store.forget_tree(&sub).unwrap();
        assert!(store.load_dir(&sub).unwrap().is_none());
        assert!(
            store
                .load_dir(&RemotePath::parse("/aXb").unwrap())
                .unwrap()
                .is_some()
        );
    }
}
