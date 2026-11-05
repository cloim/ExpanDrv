//! 상주 에이전트: IPC 서버를 띄우고 마운트된 드라이브를 소유합니다.
//!
//! 드라이브마다 독립적으로 마운트와 해제를 처리하므로 한 드라이브의 실패가 다른 드라이브에 번지지 않습니다.

use crate::{cache_dir, ipc_err, serial};
use expandrv_core::config::{Config, DriveConfig};
use expandrv_core::{Error, Result};
use expandrv_ipc::{
    AgentStatus, DriveState, DriveStatus, ErrorCode, Handler, IpcServer, Request, Response,
};
use expandrv_secrets::Secrets;
use expandrv_vfs::{Vfs, VfsOptions};
use expandrv_winfsp::{MountOptions, MountedDrive};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::Notify;

/// 마운트 해제 때 남은 업로드를 기다리는 최대 시간. 넘기면 저널로 다음 마운트에 넘깁니다.
const UPLOAD_DRAIN_TIMEOUT: Duration = Duration::from_secs(120);

struct Mounted {
    /// 마운트할 때의 설정. 다시 읽은 설정과 달라도 실제 상태를 보여 줍니다.
    cfg: DriveConfig,
    drive: MountedDrive,
    vfs: Vfs,
}

#[derive(Default)]
struct State {
    config: Config,
    mounted: HashMap<String, Mounted>,
    failed: HashMap<String, String>,
    /// 마운트나 해제가 진행 중인 드라이브. 해제 중이면 남은 업로드 수를 보이도록 VFS를 둡니다.
    busy: HashMap<String, Option<Vfs>>,
    shutting_down: bool,
}

pub struct Agent {
    cfg_path: PathBuf,
    rt: Handle,
    state: Mutex<State>,
    stop: Notify,
}

fn connect_and_mount(d: &DriveConfig, rt: &Handle) -> Result<(MountedDrive, Vfs)> {
    let backend = expandrv_vfs::throttle(
        rt.block_on(expandrv_backends::connect(
            &d.backend,
            d.proxy.as_ref(),
            &Secrets,
        ))?,
        d.transfer.upload_kib_per_sec * 1024,
        d.transfer.download_kib_per_sec * 1024,
    );
    let vfs = Vfs::new(
        backend,
        rt.clone(),
        VfsOptions {
            read_only: d.read_only,
            metadata_ttl: Duration::from_secs(d.cache.metadata_ttl_secs),
            block_size: u64::from(d.cache.block_size_mib) << 20,
            memory_cache_bytes: (d.cache.memory_limit_mib as usize) << 20,
            meta_db: Some(cache_dir(&d.id)?.join("meta.db")),
            ..VfsOptions::new(cache_dir(&d.id)?.join("staging"))
        },
    )?;
    let drive = MountedDrive::mount(
        Arc::new(vfs.clone()),
        &MountOptions {
            mount_point: d.mount_point.clone(),
            label: d.label.clone(),
            file_info_timeout_ms: 1000,
            serial: serial(&d.id),
        },
    )?;
    vfs.start_watch(Duration::from_secs(d.cache.poll_interval_secs));
    Ok((drive, vfs))
}

/// 새 쓰기를 막은 뒤(드라이브 먼저 해제) 남은 업로드를 마칩니다.
/// 끝내지 못한 업로드는 멈추고 저널에 남겨 다음 마운트 때 이어 올립니다.
fn release(id: &str, m: Mounted) -> Result<()> {
    drop(m.drive);
    let pending = m.vfs.pending_uploads();
    if pending > 0 {
        tracing::info!(id, pending, "남은 업로드를 마치는 중");
    }
    let drained = m.vfs.sync(UPLOAD_DRAIN_TIMEOUT);
    m.vfs.shutdown();
    if let Err(e) = &drained {
        tracing::warn!(id, error = %e, "업로드를 마치지 못해 다음 마운트 때 이어 올립니다");
    }
    drained
}

fn joined<T>(r: std::result::Result<Result<T>, tokio::task::JoinError>) -> Result<T> {
    r.unwrap_or_else(|e| {
        Err(Error::Io(std::io::Error::other(format!(
            "작업이 비정상 종료되었습니다: {e}"
        ))))
    })
}

impl Agent {
    fn new(cfg_path: PathBuf, config: Config, rt: Handle) -> Self {
        Self {
            cfg_path,
            rt,
            state: Mutex::new(State {
                config,
                ..State::default()
            }),
            stop: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn status(&self) -> AgentStatus {
        let st = self.lock();
        let mut seen = HashSet::new();
        let drives = st
            .config
            .drives
            .iter()
            .chain(st.mounted.values().map(|m| &m.cfg))
            .filter(|d| seen.insert(d.id.as_str()))
            .map(|d| {
                let (d, state, pending) = if let Some(m) = st.mounted.get(&d.id) {
                    (&m.cfg, DriveState::Mounted, m.vfs.pending_uploads())
                } else if let Some(Some(vfs)) = st.busy.get(&d.id) {
                    (d, DriveState::Unmounted, vfs.pending_uploads())
                } else if let Some(msg) = st.failed.get(&d.id) {
                    (d, DriveState::Failed(msg.clone()), 0)
                } else {
                    (d, DriveState::Unmounted, 0)
                };
                DriveStatus {
                    id: d.id.clone(),
                    label: d.label.clone(),
                    mount_point: d.mount_point.clone(),
                    backend: d.backend.kind().into(),
                    state,
                    pending_uploads: pending as u64,
                }
            })
            .collect();
        AgentStatus { drives }
    }

    async fn mount(&self, id: &str) -> Response {
        let d = {
            let mut st = self.lock();
            if st.shutting_down {
                return Response::error(ErrorCode::ShuttingDown, "에이전트가 종료 중입니다");
            }
            let Some(d) = st.config.drive(id).cloned() else {
                return Response::error(
                    ErrorCode::UnknownDrive,
                    format!("드라이브 {id}가 없습니다"),
                );
            };
            if st.busy.contains_key(id) {
                return Response::error(
                    ErrorCode::Busy,
                    format!("{id}에 다른 작업이 진행 중입니다"),
                );
            }
            if st.mounted.contains_key(id) {
                return Response::error(
                    ErrorCode::AlreadyMounted,
                    format!("{id}는 이미 마운트되어 있습니다"),
                );
            }
            if let Some(other) = st
                .mounted
                .values()
                .find(|m| m.cfg.mount_point.eq_ignore_ascii_case(&d.mount_point))
            {
                return Response::error(
                    ErrorCode::MountFailed,
                    format!(
                        "{}을 드라이브 {}가 쓰고 있습니다",
                        d.mount_point, other.cfg.id
                    ),
                );
            }
            st.busy.insert(id.to_owned(), None);
            d
        };
        let (rt, cfg) = (self.rt.clone(), d.clone());
        let r = joined(tokio::task::spawn_blocking(move || connect_and_mount(&cfg, &rt)).await);
        let mut st = self.lock();
        st.busy.remove(id);
        match r {
            Ok((drive, vfs)) => {
                tracing::info!(id, mount_point = %d.mount_point, "마운트");
                st.failed.remove(id);
                st.mounted
                    .insert(id.to_owned(), Mounted { cfg: d, drive, vfs });
                Response::Ok
            }
            Err(e) => {
                let msg = e.to_string();
                tracing::warn!(id, "마운트 실패: {msg}");
                st.failed.insert(id.to_owned(), msg.clone());
                Response::error(ErrorCode::MountFailed, msg)
            }
        }
    }

    async fn unmount(&self, id: &str) -> Response {
        let m = {
            let mut st = self.lock();
            if st.busy.contains_key(id) {
                return Response::error(
                    ErrorCode::Busy,
                    format!("{id}에 다른 작업이 진행 중입니다"),
                );
            }
            let Some(m) = st.mounted.remove(id) else {
                return Response::error(
                    ErrorCode::NotMounted,
                    format!("{id}는 마운트되어 있지 않습니다"),
                );
            };
            st.busy.insert(id.to_owned(), Some(m.vfs.clone()));
            m
        };
        let owned = id.to_owned();
        let r = joined(tokio::task::spawn_blocking(move || release(&owned, m)).await);
        let mut st = self.lock();
        st.busy.remove(id);
        match r {
            Ok(()) => {
                tracing::info!(id, "해제");
                st.failed.remove(id);
                Response::Ok
            }
            Err(e) => {
                let msg = e.to_string();
                tracing::warn!(id, "해제 후 업로드 실패: {msg}");
                st.failed.insert(id.to_owned(), msg.clone());
                Response::error(ErrorCode::UnmountFailed, msg)
            }
        }
    }

    fn reload(&self) -> Response {
        match Config::load(&self.cfg_path) {
            Ok(cfg) => {
                let mut st = self.lock();
                st.failed.retain(|id, _| cfg.drive(id).is_some());
                st.config = cfg;
                tracing::info!("설정을 다시 읽었습니다");
                Response::Ok
            }
            Err(e) => Response::error(ErrorCode::Config, e.to_string()),
        }
    }

    /// 새 작업을 막고 진행 중인 작업을 기다린 뒤 모든 드라이브를 함께 해제합니다.
    async fn unmount_all(&self) -> Vec<String> {
        self.lock().shutting_down = true;
        while !self.lock().busy.is_empty() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let jobs: Vec<_> = {
            let mut st = self.lock();
            let all: Vec<_> = st.mounted.drain().collect();
            all.into_iter()
                .map(|(id, m)| {
                    st.busy.insert(id.clone(), Some(m.vfs.clone()));
                    let owned = id.clone();
                    (id, tokio::task::spawn_blocking(move || release(&owned, m)))
                })
                .collect()
        };
        let mut errors = Vec::new();
        for (id, job) in jobs {
            let r = joined(job.await);
            self.lock().busy.remove(&id);
            match r {
                Ok(()) => tracing::info!(id, "해제"),
                Err(e) => {
                    tracing::warn!(id, "해제 후 업로드 실패: {e}");
                    errors.push(format!("{id}: {e}"));
                }
            }
        }
        errors
    }
}

#[async_trait::async_trait]
impl Handler for Agent {
    fn agent_version(&self) -> String {
        env!("CARGO_PKG_VERSION").into()
    }

    async fn handle(&self, req: Request) -> Response {
        match req {
            // Hello는 IPC 서버가 직접 답합니다.
            Request::Hello { .. } => Response::Ok,
            Request::Status => Response::Status(self.status()),
            Request::Mount { id } => self.mount(&id).await,
            Request::Unmount { id } => self.unmount(&id).await,
            Request::Reload => self.reload(),
            Request::Shutdown => {
                let errors = self.unmount_all().await;
                self.stop.notify_one();
                if errors.is_empty() {
                    Response::Ok
                } else {
                    Response::error(ErrorCode::UnmountFailed, errors.join("; "))
                }
            }
        }
    }
}

/// `expandrv agent`: 사용자당 하나만 실행합니다. Shutdown 요청이나 Ctrl+C로 끝납니다.
/// `autostart`면 시작 직후 `auto_mount` 드라이브를 마운트합니다(로그인 자동 실행용).
pub fn run(cfg_path: PathBuf, pipe: &str, autostart: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        // 단일 인스턴스를 먼저 확인합니다.
        let server = IpcServer::bind(pipe).map_err(ipc_err)?;
        let config = Config::load(&cfg_path)?;
        let auto: Vec<String> = config
            .drives
            .iter()
            .filter(|d| autostart && d.auto_mount)
            .map(|d| d.id.clone())
            .collect();
        let agent = Arc::new(Agent::new(cfg_path, config, Handle::current()));
        tracing::info!(pid = std::process::id(), autostart, "에이전트 시작");
        for id in auto {
            let a = agent.clone();
            tokio::spawn(async move {
                match a.mount(&id).await {
                    Response::Ok => tracing::info!(%id, "자동 마운트"),
                    other => tracing::warn!(%id, ?other, "자동 마운트 실패"),
                }
            });
        }
        println!("에이전트를 시작했습니다. Ctrl+C 또는 `expandrv stop`으로 끝냅니다.");
        let a = agent.clone();
        let shutdown = async move {
            let ctrl_c = async {
                if tokio::signal::ctrl_c().await.is_err() {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                _ = a.stop.notified() => {}
                _ = ctrl_c => tracing::info!("Ctrl+C 수신"),
            }
        };
        let served = server.serve(agent.clone(), shutdown).await;
        // 서버가 오류로 끝나도 드라이브는 업로드를 마치고 해제합니다.
        let errors = agent.unmount_all().await;
        tracing::info!("에이전트 종료");
        served.map_err(ipc_err)?;
        if errors.is_empty() {
            Ok(())
        } else {
            Err(Error::Io(std::io::Error::other(errors.join("; "))))
        }
    })
}
