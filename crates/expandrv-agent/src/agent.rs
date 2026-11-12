//! 상주 에이전트: IPC 서버를 띄우고 마운트된 드라이브를 소유합니다.
//!
//! 드라이브마다 독립적으로 마운트와 해제를 처리하므로 한 드라이브의 실패가 다른 드라이브에 번지지 않습니다.

use crate::auto_lock::{self, Reason};
use crate::{cache_dir, ipc_err, serial};
use expandrv_core::config::{Config, DriveConfig, NotificationKind};
use expandrv_core::{Error, Result};
use expandrv_ipc::{
    AgentStatus, DriveState, DriveStatus, ErrorCode, Handler, IpcServer, Request, Response,
};
use expandrv_secrets::lock::PinStore;
use expandrv_secrets::{CredentialStore, Secrets, target_for};
use expandrv_vfs::{DiskCacheOptions, Vfs, VfsOptions};
use expandrv_winfsp::{MountOptions, MountedDrive};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
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
    locked: bool,
    locking: bool,
    lock_reason: Option<Reason>,
    session_restore_drives: Vec<String>,
    session_locked: bool,
    pin_configured: bool,
    pin_failures: u32,
    pin_retry: Option<Instant>,
    activity_anchor: Option<Instant>,
}

pub struct Agent {
    cfg_path: PathBuf,
    rt: Handle,
    state: Mutex<State>,
    stop: Arc<Notify>,
    lock_requested: Arc<Notify>,
    session_lock_requested: Arc<Notify>,
    session_unlock_requested: Arc<Notify>,
    transitions: tokio::sync::Mutex<()>,
    tray: Option<crate::tray::Tray>,
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
    // 원격 파일 이름과 내용이 남는 폴더이므로 현재 사용자와 SYSTEM만 접근하게 합니다.
    let dir = cache_dir(&d.id)?;
    std::fs::create_dir_all(&dir)?;
    expandrv_winfsp::security::protect_dir(&dir)?;
    let disk_cache = match d.cache.disk_limit_mib {
        0 => None,
        mib => match cache_key(&d.id) {
            Ok(key) => Some(DiskCacheOptions {
                dir: dir.join("blocks"),
                max_bytes: mib << 20,
                key,
            }),
            Err(e) => {
                tracing::warn!(id = %d.id, error = %e, "캐시 키를 준비하지 못해 디스크 캐시 없이 마운트합니다");
                None
            }
        },
    };
    let vfs = Vfs::new(
        backend,
        rt.clone(),
        VfsOptions {
            read_only: d.read_only,
            metadata_ttl: Duration::from_secs(d.cache.metadata_ttl_secs),
            block_size: u64::from(d.cache.block_size_mib) << 20,
            memory_cache_bytes: (d.cache.memory_limit_mib as usize) << 20,
            readahead_blocks: d.cache.readahead_blocks,
            meta_db: Some(dir.join("meta.db")),
            disk_cache,
            ..VfsOptions::new(dir.join("staging"))
        },
    )?;
    let drive = MountedDrive::mount(
        Arc::new(vfs.clone()),
        &MountOptions {
            mount_point: d.mount_point.clone(),
            label: d.label.clone(),
            file_info_timeout_ms: 1000,
            serial: serial(&d.id),
            network_share: d.network_drive.then(|| d.id.clone()),
        },
    )?;
    vfs.start_watch(Duration::from_secs(d.cache.poll_interval_secs));
    Ok((drive, vfs))
}

/// 드라이브별 디스크 캐시 키. 자격 증명 관리자(`ExpanDrv/<id>/cache_key`)에 16진수로 두고 없으면 만듭니다.
fn cache_key(id: &str) -> Result<[u8; 32]> {
    use secrecy::{ExposeSecret, SecretString};
    let store = CredentialStore;
    let target = target_for(id, "cache_key");
    match store.get(&target) {
        Ok(hex) => {
            let hex = hex.expose_secret();
            let mut key = [0u8; 32];
            if hex.len() != 64 {
                return Err(Error::Config(format!("{target} 형식이 올바르지 않습니다")));
            }
            for (i, b) in key.iter_mut().enumerate() {
                *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                    .map_err(|_| Error::Config(format!("{target} 형식이 올바르지 않습니다")))?;
            }
            Ok(key)
        }
        Err(Error::NotFound) => {
            let key = expandrv_vfs::generate_cache_key()?;
            let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
            store.set(&target, &SecretString::from(hex))?;
            Ok(key)
        }
        Err(e) => Err(e),
    }
}

/// 새 쓰기를 막은 뒤(드라이브 먼저 해제) 남은 업로드를 마칩니다.
/// 끝내지 못한 업로드는 멈추고 저널에 남겨 다음 마운트 때 이어 올립니다.
fn release(id: &str, m: Mounted, tray: Option<crate::tray::Tray>) -> Result<()> {
    drop(m.drive);
    let pending = m.vfs.pending_uploads();
    if pending > 0 {
        tracing::info!(id, pending, "남은 업로드를 마치는 중");
    }
    let drained = m.vfs.sync(UPLOAD_DRAIN_TIMEOUT);
    if let Some(tray) = tray {
        tray.transfers(id, &m.vfs);
    }
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
    fn new(cfg_path: PathBuf, config: Config, rt: Handle) -> Result<Self> {
        let pins = PinStore::new(&cfg_path)?;
        let pin_configured = pins.configured()?;
        // PIN으로 해제한 상태는 재시작 뒤에도 유지하고, 사용자가 잠근 상태만 잠긴 채로 시작합니다.
        let start_locked = pin_configured && pins.remembered_locked();
        let stop = Arc::new(Notify::new());
        let lock_requested = Arc::new(Notify::new());
        let session_lock_requested = Arc::new(Notify::new());
        let session_unlock_requested = Arc::new(Notify::new());
        let tray = crate::tray::Tray::start(
            rt.clone(),
            stop.clone(),
            lock_requested.clone(),
            session_lock_requested.clone(),
            session_unlock_requested.clone(),
            config.notifications,
        )
        .map_err(|e| tracing::warn!(error = %e, "트레이 스레드 시작 실패"))
        .ok();
        Ok(Self {
            cfg_path,
            rt,
            state: Mutex::new(State {
                config,
                pin_configured,
                locked: start_locked,
                lock_reason: if start_locked {
                    Some(Reason::Manual)
                } else {
                    None
                },
                activity_anchor: Some(Instant::now()),
                ..State::default()
            }),
            stop,
            lock_requested,
            session_lock_requested,
            session_unlock_requested,
            tray,
            transitions: tokio::sync::Mutex::new(()),
        })
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
                let (d, state, pending, downloads) = if let Some(m) = st.mounted.get(&d.id) {
                    let v = &m.vfs;
                    (
                        &m.cfg,
                        DriveState::Mounted,
                        v.pending_uploads(),
                        v.active_downloads(),
                    )
                } else if let Some(Some(vfs)) = st.busy.get(&d.id) {
                    (d, DriveState::Unmounted, vfs.pending_uploads(), 0)
                } else if let Some(msg) = st.failed.get(&d.id) {
                    (d, DriveState::Failed(msg.clone()), 0, 0)
                } else {
                    (d, DriveState::Unmounted, 0, 0)
                };
                DriveStatus {
                    id: d.id.clone(),
                    label: d.label.clone(),
                    mount_point: d.mount_point.clone(),
                    backend: d.backend.kind().into(),
                    state,
                    pending_uploads: pending as u64,
                    active_downloads: downloads as u64,
                }
            })
            .collect();
        AgentStatus {
            drives,
            locked: st.locked,
            locking: st.locking,
            pin_configured: st.pin_configured,
        }
    }

    async fn mount(&self, id: &str) -> Response {
        let d = loop {
            let ready = {
                let st = self.lock();
                // 시작 시 자동 마운트처럼 같은 드라이브를 마운트하는 중이면 끝난 뒤 결과를 따릅니다.
                st.shutting_down || !matches!(st.busy.get(id), Some(None))
            };
            if ready {
                break self.begin_mount(id);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let d = match d {
            Ok(d) => d,
            Err(response) => return response,
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

    /// 마운트할 수 있는지 확인하고 작업 중으로 표시합니다.
    fn begin_mount(&self, id: &str) -> std::result::Result<DriveConfig, Response> {
        let mut st = self.lock();
        if st.shutting_down {
            return Err(Response::error(
                ErrorCode::ShuttingDown,
                "에이전트가 종료 중입니다",
            ));
        }
        if st.locked {
            return Err(Response::error(
                ErrorCode::Locked,
                "드라이브가 잠겨 있습니다. PIN으로 잠금을 해제하세요",
            ));
        }
        let Some(d) = st.config.drive(id).cloned() else {
            return Err(Response::error(
                ErrorCode::UnknownDrive,
                format!("드라이브 {id}가 없습니다"),
            ));
        };
        if st.busy.contains_key(id) {
            return Err(Response::error(
                ErrorCode::Busy,
                format!("{id}에 다른 작업이 진행 중입니다"),
            ));
        }
        if st.mounted.contains_key(id) {
            return Err(Response::error(
                ErrorCode::AlreadyMounted,
                format!("{id}는 이미 마운트되어 있습니다"),
            ));
        }
        if let Some(other) = st
            .mounted
            .values()
            .find(|m| m.cfg.mount_point.eq_ignore_ascii_case(&d.mount_point))
        {
            return Err(Response::error(
                ErrorCode::MountFailed,
                format!(
                    "{}을 드라이브 {}가 쓰고 있습니다",
                    d.mount_point, other.cfg.id
                ),
            ));
        }
        st.busy.insert(id.to_owned(), None);
        Ok(d)
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
        let tray = self.tray.clone();
        let r = joined(tokio::task::spawn_blocking(move || release(&owned, m, tray)).await);
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
                if st.config.security != cfg.security {
                    st.activity_anchor = Some(Instant::now());
                }
                let policy_disabled =
                    st.config.security.lock_on_session_lock && !cfg.security.lock_on_session_lock;
                if let Some(tray) = &self.tray {
                    tray.set_notifications(cfg.notifications);
                }
                st.config = cfg;
                if policy_disabled && st.locked && st.lock_reason == Some(Reason::Session) {
                    let to_restore = std::mem::take(&mut st.session_restore_drives);
                    st.locked = false;
                    st.lock_reason = None;
                    drop(st);
                    for id in to_restore {
                        let a = self.rt.clone();
                        // 드라이브 복구를 비동기 예약합니다.
                        let id_clone = id.clone();
                        let a_self = self;
                        let _ = a.block_on(async { a_self.mount(&id_clone).await });
                    }
                    if let Some(tray) = &self.tray {
                        tray.status(self.status());
                        tray.notice(
                            NotificationKind::LockRecovery,
                            "잠금 해제",
                            "세션 잠금 정책 해제로 드라이브를 복구했습니다",
                        );
                    }
                    tracing::info!("세션 잠금 정책 해제로 드라이브를 복구했습니다");
                    return Response::Ok;
                }
                tracing::info!("설정을 다시 읽었습니다");
                Response::Ok
            }
            Err(e) => Response::error(ErrorCode::Config, e.to_string()),
        }
    }

    async fn lock_drives(&self, reason: Reason) -> Response {
        let _guard = self.transitions.lock().await;
        let should_drain = {
            let mut st = self.lock();
            if st.shutting_down {
                return Response::error(ErrorCode::ShuttingDown, "에이전트가 종료 중입니다");
            }
            match reason {
                Reason::Session => {
                    st.session_locked = true;
                    if !st.config.security.lock_on_session_lock {
                        return Response::Ok;
                    }
                    if st.locked {
                        return Response::Ok;
                    }
                    st.session_restore_drives = st.mounted.keys().cloned().collect();
                    st.locked = true;
                    st.locking = true;
                    st.lock_reason = Some(Reason::Session);
                    tracing::info!(?reason, drives = ?st.session_restore_drives, "Windows 세션 잠금으로 드라이브 잠금 시작");
                    true
                }
                Reason::Manual => {
                    if !st.pin_configured {
                        return Response::error(
                            ErrorCode::Config,
                            "설정에서 잠금 PIN을 먼저 등록하세요",
                        );
                    }
                    if st.locked && st.lock_reason == Some(Reason::Manual) {
                        return Response::Ok;
                    }
                    st.session_restore_drives.clear();
                    st.locked = true;
                    st.locking = true;
                    st.lock_reason = Some(Reason::Manual);
                    tracing::info!(?reason, "수동 PIN 잠금 시작");
                    self.remember_locked(true);
                    true
                }
                Reason::Idle => {
                    if !st.pin_configured
                        || st.locked
                        || !auto_lock::due(
                            &st.config.security,
                            reason,
                            auto_lock::idle_time(),
                            st.activity_anchor.map_or(Duration::ZERO, |at| at.elapsed()),
                        )
                    {
                        return Response::Ok;
                    }
                    st.session_restore_drives.clear();
                    st.locked = true;
                    st.locking = true;
                    st.lock_reason = Some(Reason::Idle);
                    tracing::info!(?reason, "유휴 시간 초과로 자동 잠금 시작");
                    self.remember_locked(true);
                    true
                }
            }
        };

        if !should_drain {
            return Response::Ok;
        }

        let errors = self.drain_all().await;

        let needs_restore = {
            let mut st = self.lock();
            st.locking = false;
            reason == Reason::Session
                && !st.session_locked
                && st.locked
                && st.lock_reason == Some(Reason::Session)
        };

        if needs_restore {
            tracing::info!("잠금 처리 중 세션 해제 이벤트가 감지되어 즉시 복구합니다");
            self.unlock_session().await;
        }

        if errors.is_empty() {
            Response::Ok
        } else {
            Response::error(
                ErrorCode::UnmountFailed,
                format!(
                    "잠금을 완료했습니다. 남은 변경분은 다음 마운트에 이어 올립니다: {}",
                    errors.join("; ")
                ),
            )
        }
    }

    async fn unlock_session(&self) {
        let _guard = self.transitions.lock().await;
        let to_restore = {
            let mut st = self.lock();
            st.session_locked = false;
            if st.locking {
                return;
            }
            if !st.locked || st.lock_reason != Some(Reason::Session) {
                // 수동 또는 유휴 잠금 상태는 Windows 세션 해제 이벤트로 임의 해제하지 않습니다.
                return;
            }
            st.locked = false;
            st.lock_reason = None;
            st.activity_anchor = Some(Instant::now());
            std::mem::take(&mut st.session_restore_drives)
        };

        tracing::info!(drives = ?to_restore, "Windows 세션 잠금 해제에 맞춰 드라이브 복구 시작");
        for id in &to_restore {
            let _ = self.mount(id).await;
        }
        if let Some(tray) = &self.tray {
            tray.status(self.status());
            tray.notice(
                NotificationKind::LockRecovery,
                "드라이브 복구",
                "Windows 잠금 해제에 맞춰 드라이브를 복구했습니다",
            );
        }
    }

    /// 사용자 잠금(수동, 유휴)을 기록합니다. Windows 세션 잠금은 해제 때 자동 복구하므로 기록하지 않습니다.
    fn remember_locked(&self, locked: bool) {
        let result = PinStore::new(&self.cfg_path).and_then(|pins| pins.remember_locked(locked));
        if let Err(e) = result {
            tracing::warn!(error = %e, "잠금 상태 기록 실패");
        }
    }

    async fn pin_action(
        &self,
        current: expandrv_ipc::Pin,
        new: Option<expandrv_ipc::Pin>,
    ) -> Response {
        let _guard = self.transitions.lock().await;
        {
            let st = self.lock();
            if st.shutting_down {
                return Response::error(ErrorCode::ShuttingDown, "에이전트가 종료 중입니다");
            }
            if st.pin_retry.is_some_and(|at| at > Instant::now()) {
                return Response::error(
                    ErrorCode::Busy,
                    "PIN을 여러 번 틀렸습니다. 30초 후 다시 입력하세요",
                );
            }
            if new.is_some() && st.locked {
                return Response::error(ErrorCode::Locked, "PIN을 바꾸려면 먼저 잠금을 해제하세요");
            }
            if new.is_none() && !st.pin_configured {
                return Response::Ok;
            }
        }
        let pins = PinStore::new(&self.cfg_path);
        let setting = new.is_some();
        let result = joined(
            tokio::task::spawn_blocking(move || {
                let pins = pins?;
                match new {
                    Some(new) => pins.set(&current.0, &new.0)?,
                    None => pins.verify(&current.0)?,
                }
                // PIN 등록과 잠금 해제 모두 해제 상태가 되므로 재시작 뒤에도 유지합니다.
                if let Err(e) = pins.remember_locked(false) {
                    tracing::warn!(error = %e, "잠금 해제 상태 기록 실패");
                }
                Ok(())
            })
            .await,
        );
        let mut st = self.lock();
        match result {
            Ok(()) => {
                st.pin_failures = 0;
                st.pin_retry = None;
                st.activity_anchor = Some(Instant::now());
                if setting {
                    st.pin_configured = true;
                } else {
                    st.locked = false;
                    st.lock_reason = None;
                    st.session_restore_drives.clear();
                }
                Response::Ok
            }
            Err(e) => {
                if matches!(e, Error::Auth(_)) {
                    st.pin_failures += 1;
                    if st.pin_failures >= 5 {
                        st.pin_retry = Some(Instant::now() + Duration::from_secs(30));
                        st.pin_failures = 0;
                    }
                }
                Response::error(ErrorCode::Authentication, e.to_string())
            }
        }
    }

    /// 새 작업을 막고 진행 중인 작업을 기다린 뒤 모든 드라이브를 함께 해제합니다.
    async fn unmount_all(&self) -> Vec<String> {
        self.lock().shutting_down = true;
        self.drain_all().await
    }

    async fn drain_all(&self) -> Vec<String> {
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
                    {
                        let tray = self.tray.clone();
                        (
                            id,
                            tokio::task::spawn_blocking(move || release(&owned, m, tray)),
                        )
                    }
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
        let update = !matches!(&req, Request::Status | Request::Hello { .. });
        let response = match req {
            // Hello는 IPC 서버가 직접 답합니다.
            Request::Hello { .. } => Response::Ok,
            Request::Status => Response::Status(self.status()),
            Request::Lock => self.lock_drives(Reason::Manual).await,
            Request::Unlock { pin } => self.pin_action(pin, None).await,
            Request::SetPin { current, new } => self.pin_action(current, Some(new)).await,
            Request::Mount { id } => self.mount(&id).await,
            Request::Unmount { id } => self.unmount(&id).await,
            Request::Reload => self.reload(),
            Request::Shutdown => {
                let _guard = self.transitions.lock().await;
                let errors = self.unmount_all().await;
                self.stop.notify_one();
                if errors.is_empty() {
                    Response::Ok
                } else {
                    Response::error(ErrorCode::UnmountFailed, errors.join("; "))
                }
            }
        };
        if update && let Some(tray) = &self.tray {
            tray.status(self.status());
        }
        response
    }
}

/// `expandrv agent`: 사용자당 하나만 실행합니다. Shutdown 요청이나 Ctrl+C로 끝납니다.
/// 어떻게 시작했든(로그인 자동 실행, GUI, CLI) 시작 직후 `auto_mount` 드라이브를 마운트합니다.
/// `autostart`는 로그인 자동 실행 여부로 로그에만 남깁니다.
pub fn run(cfg_path: PathBuf, pipe: &str, autostart: bool) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        // 단일 인스턴스를 먼저 확인합니다.
        let server = IpcServer::bind(pipe).map_err(ipc_err)?;
        let config = Config::load(&cfg_path)?;
        let auto: Vec<String> = config
            .drives
            .iter()
            .filter(|d| d.auto_mount)
            .map(|d| d.id.clone())
            .collect();
        let agent = Arc::new(Agent::new(cfg_path, config, Handle::current())?);
        tracing::info!(pid = std::process::id(), autostart, "에이전트 시작");
        let lock_agent = agent.clone();
        let lock_monitor = tokio::spawn(async move {
            loop {
                lock_agent.lock_requested.notified().await;
                let response = lock_agent.lock_drives(Reason::Manual).await;
                if let Some(tray) = &lock_agent.tray {
                    tray.status(lock_agent.status());
                    match response {
                        Response::Ok => tray.notice(
                            NotificationKind::LockRecovery,
                            "드라이브 잠금",
                            "모든 드라이브를 잠갔습니다",
                        ),
                        Response::Error { message, .. } => {
                            tray.notice(NotificationKind::LockRecovery, "드라이브 잠금", &message)
                        }
                        _ => {}
                    }
                }
            }
        });
        let auto_agent = agent.clone();
        let auto_monitor = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                enum Event {
                    SessionLock,
                    SessionUnlock,
                    Idle,
                }
                let event = tokio::select! {
                    _ = auto_agent.session_lock_requested.notified() => Event::SessionLock,
                    _ = auto_agent.session_unlock_requested.notified() => Event::SessionUnlock,
                    _ = tick.tick() => Event::Idle,
                };
                match event {
                    Event::SessionUnlock => {
                        auto_agent.unlock_session().await;
                    }
                    Event::SessionLock => {
                        let response = auto_agent.lock_drives(Reason::Session).await;
                        if let Some(tray) = &auto_agent.tray {
                            let status = auto_agent.status();
                            if status.locked {
                                tray.status(status);
                                match response {
                                    Response::Error { message, .. } => tray.notice(
                                        NotificationKind::LockRecovery,
                                        "자동 잠금",
                                        &message,
                                    ),
                                    _ => tray.notice(
                                        NotificationKind::LockRecovery,
                                        "자동 잠금",
                                        "Windows 잠금에 맞춰 드라이브를 잠갔습니다",
                                    ),
                                }
                            }
                        }
                    }
                    Event::Idle => {
                        if auto_agent.lock().locked {
                            continue;
                        }
                        let response = auto_agent.lock_drives(Reason::Idle).await;
                        if let Some(tray) = &auto_agent.tray {
                            let status = auto_agent.status();
                            if status.locked {
                                tray.status(status);
                                match response {
                                    Response::Error { message, .. } => tray.notice(
                                        NotificationKind::LockRecovery,
                                        "자동 잠금",
                                        &message,
                                    ),
                                    _ => tray.notice(
                                        NotificationKind::LockRecovery,
                                        "자동 잠금",
                                        "유휴 시간이 지나 드라이브를 잠갔습니다",
                                    ),
                                }
                            }
                        }
                    }
                }
            }
        });
        let weak = Arc::downgrade(&agent);
        let monitor = tokio::spawn(async move {
            loop {
                let Some(agent) = weak.upgrade() else { break };
                if let Some(tray) = &agent.tray {
                    tray.status(agent.status());
                    let st = agent.lock();
                    for (id, mounted) in &st.mounted {
                        tray.transfers(id, &mounted.vfs);
                    }
                    for (id, vfs) in &st.busy {
                        if let Some(vfs) = vfs {
                            tray.transfers(id, vfs);
                        }
                    }
                }
                drop(agent);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
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
        if let Some(tray) = &agent.tray {
            tray.status(agent.status());
        }
        monitor.abort();
        lock_monitor.abort();
        auto_monitor.abort();
        if let Some(tray) = &agent.tray {
            tray.shutdown();
        }
        tracing::info!("에이전트 종료");
        served.map_err(ipc_err)?;
        if errors.is_empty() {
            Ok(())
        } else {
            Err(Error::Io(std::io::Error::other(errors.join("; "))))
        }
    })
}
