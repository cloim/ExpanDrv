//! 화면이 호출하는 명령. 목록은 build.rs의 COMMANDS와 capabilities/main.json에 같게 둡니다.

use crate::agent::{self, Result};
use crate::drives::{self, BackendInput, DriveDetail, DrivePatch, NewDrive, ProxyInput, Secret};
use expandrv_core::RemotePath;
use expandrv_core::config::{BackendConfig, Config, ProxyConfig, SecretResolver};
use expandrv_ipc::{AgentStatus, DriveState, Request};
use expandrv_secrets::{CredentialStore, Secrets};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tauri::State;

/// 연결 확인 제한 시간
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Paths {
    pub config: PathBuf,
    pub pipe: String,
    pub cli: PathBuf,
    pub log_dir: PathBuf,
}

impl Paths {
    /// CLI와 같은 기본 위치를 씁니다. `EXPANDRV_PIPE`, `EXPANDRV_CLI`로 바꿀 수 있습니다(개발과 테스트용).
    pub fn detect() -> std::result::Result<Self, String> {
        let env_dir = |var: &str| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .ok_or_else(|| format!("{var} 환경 변수가 없습니다"))
        };
        let pipe = match std::env::var("EXPANDRV_PIPE") {
            Ok(p) => p,
            Err(_) => expandrv_ipc::default_pipe_name().map_err(|e| e.to_string())?,
        };
        let cli = match std::env::var_os("EXPANDRV_CLI") {
            Some(p) => PathBuf::from(p),
            None => std::env::current_exe()
                .map_err(|e| e.to_string())?
                .with_file_name("expandrv.exe"),
        };
        Ok(Self {
            config: env_dir("APPDATA")?.join("ExpanDrv").join("config.toml"),
            pipe,
            cli,
            log_dir: env_dir("LOCALAPPDATA")?.join("ExpanDrv").join("logs"),
        })
    }
}

pub struct AppState {
    pub paths: Paths,
    /// 설정 파일 읽기, 고치기, 쓰기를 한 번에 하나만 합니다.
    pub config_lock: Mutex<()>,
}

impl AppState {
    fn edit<T>(&self, f: impl FnOnce(&Path) -> expandrv_core::Result<T>) -> Result<T> {
        let _g = self.config_lock.lock().unwrap_or_else(|e| e.into_inner());
        f(&self.paths.config).map_err(|e| e.to_string())
    }

    fn load(&self) -> Result<Config> {
        self.edit(Config::load)
    }

    /// 설정을 바꾼 뒤 실행 중인 에이전트가 있으면 다시 읽게 합니다.
    async fn reload(&self) -> Result<()> {
        if let Some(mut link) = agent::connect(&self.paths.pipe).await? {
            link.call_ok(&Request::Reload)
                .await
                .map_err(|e| format!("저장했지만 에이전트 설정 갱신에 실패했습니다: {e}"))?;
        }
        Ok(())
    }

    async fn start_agent(&self) -> Result<agent::Link> {
        let p = &self.paths;
        let mut link = agent::start(&p.pipe, &p.config, &p.cli, &p.log_dir).await?;
        // 이미 떠 있던 에이전트도 최신 설정을 쓰게 합니다.
        link.call_ok(&Request::Reload).await?;
        Ok(link)
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveRow {
    pub id: String,
    pub label: String,
    pub mount_point: String,
    pub backend: String,
    /// `mounted`, `unmounted`, `failed`
    pub state: &'static str,
    pub message: Option<String>,
    pub pending_uploads: u64,
    /// 원격에서 받고 있는 블록 수
    pub active_downloads: u64,
    pub auto_mount: bool,
    pub read_only: bool,
    /// 설정에 없는데 에이전트가 아직 들고 있는 드라이브면 false
    pub configured: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusView {
    pub running: bool,
    pub agent_version: Option<String>,
    pub config_error: Option<String>,
    pub drives: Vec<DriveRow>,
}

fn state_of(s: &DriveState) -> (&'static str, Option<String>) {
    match s {
        DriveState::Mounted => ("mounted", None),
        DriveState::Unmounted => ("unmounted", None),
        DriveState::Failed(m) => ("failed", Some(m.clone())),
    }
}

/// 설정의 드라이브 목록에 에이전트 상태를 합칩니다.
pub fn merge_status(cfg: Option<&Config>, status: &AgentStatus) -> Vec<DriveRow> {
    let mut rows: Vec<DriveRow> = cfg
        .map(|c| c.drives.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|d| {
            let live = status.drives.iter().find(|s| s.id == d.id);
            let (state, message) = live.map_or(("unmounted", None), |s| state_of(&s.state));
            DriveRow {
                id: d.id.clone(),
                label: d.label.clone(),
                mount_point: d.mount_point.clone(),
                backend: d.backend.kind().to_owned(),
                state,
                message,
                pending_uploads: live.map_or(0, |s| s.pending_uploads),
                active_downloads: live.map_or(0, |s| s.active_downloads),
                auto_mount: d.auto_mount,
                read_only: d.read_only,
                configured: true,
            }
        })
        .collect();
    for s in &status.drives {
        if rows.iter().any(|r| r.id == s.id) {
            continue;
        }
        let (state, message) = state_of(&s.state);
        rows.push(DriveRow {
            id: s.id.clone(),
            label: s.label.clone(),
            mount_point: s.mount_point.clone(),
            backend: s.backend.clone(),
            state,
            message,
            pending_uploads: s.pending_uploads,
            active_downloads: s.active_downloads,
            auto_mount: false,
            read_only: false,
            configured: false,
        });
    }
    rows
}

pub async fn status_view(state: &AppState) -> Result<StatusView> {
    let (cfg, config_error) = match state.load() {
        Ok(c) => (Some(c), None),
        Err(e) => (None, Some(e)),
    };
    let (running, agent_version, status) = match agent::connect(&state.paths.pipe).await? {
        Some(mut link) => {
            let s = link.status().await?;
            (true, Some(link.version), s)
        }
        None => (false, None, AgentStatus::default()),
    };
    Ok(StatusView {
        running,
        agent_version,
        config_error,
        drives: merge_status(cfg.as_ref(), &status),
    })
}

#[tauri::command]
pub async fn agent_status(state: State<'_, AppState>) -> Result<StatusView> {
    status_view(&state).await
}

#[tauri::command]
pub async fn mount_drive(state: State<'_, AppState>, id: String) -> Result<()> {
    drives::check_id(&id).map_err(|e| e.to_string())?;
    if state.load()?.drive(&id).is_none() {
        return Err(format!("드라이브 {id}가 없습니다"));
    }
    state.start_agent().await?.mount(&id).await
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MountResult {
    pub id: String,
    pub error: Option<String>,
}

/// 설정된 모든 드라이브를 마운트합니다(CLI `expandrv mount`와 같음).
#[tauri::command]
pub async fn mount_all(state: State<'_, AppState>) -> Result<Vec<MountResult>> {
    let cfg = state.load()?;
    if cfg.drives.is_empty() {
        return Err("마운트할 드라이브가 없습니다".into());
    }
    let mut link = state.start_agent().await?;
    let mut out = Vec::new();
    for d in &cfg.drives {
        out.push(MountResult {
            id: d.id.clone(),
            error: link.mount(&d.id).await.err(),
        });
    }
    Ok(out)
}

#[tauri::command]
pub async fn unmount_drive(state: State<'_, AppState>, id: String) -> Result<()> {
    drives::check_id(&id).map_err(|e| e.to_string())?;
    unmount(&state.paths.pipe, &id).await
}

pub async fn unmount(pipe: &str, id: &str) -> Result<()> {
    agent::require(pipe)
        .await?
        .call_ok(&Request::Unmount { id: id.to_owned() })
        .await
}

#[tauri::command]
pub async fn get_drive(state: State<'_, AppState>, id: String) -> Result<DriveDetail> {
    let cfg = state.load()?;
    let d = cfg
        .drive(&id)
        .ok_or_else(|| format!("드라이브 {id}가 없습니다"))?;
    let stored = CredentialStore
        .list(&format!("ExpanDrv/{id}/"))
        .map_err(|e| e.to_string())?;
    Ok(drives::drive_detail(d, &stored))
}

#[tauri::command]
pub async fn add_drive(state: State<'_, AppState>, drive: NewDrive) -> Result<String> {
    let id = state.edit(|p| drives::add_drive(p, &CredentialStore, drive))?;
    state.reload().await?;
    Ok(id)
}

#[tauri::command]
pub async fn update_drive(state: State<'_, AppState>, id: String, patch: DrivePatch) -> Result<()> {
    state.edit(|p| drives::update_drive(p, &CredentialStore, &id, patch))?;
    state.reload().await
}

#[tauri::command]
pub async fn set_drive_secret(
    state: State<'_, AppState>,
    id: String,
    field: String,
    value: Secret,
) -> Result<()> {
    state.edit(|p| drives::set_drive_secret(p, &CredentialStore, &id, &field, value))?;
    state.reload().await
}

/// 마운트 중이면 거부합니다. 설정과 함께 이 드라이브의 자격 증명도 지웁니다.
#[tauri::command]
pub async fn delete_drive(state: State<'_, AppState>, id: String) -> Result<usize> {
    if let Some(mut link) = agent::connect(&state.paths.pipe).await?
        && link
            .status()
            .await?
            .drives
            .iter()
            .any(|d| d.id == id && d.state == DriveState::Mounted)
    {
        return Err(format!("{id}가 마운트되어 있습니다. 먼저 해제하세요"));
    }
    let n = state.edit(|p| drives::delete_drive(p, &CredentialStore, &id))?;
    state.reload().await?;
    Ok(n)
}

async fn probe(
    backend: &BackendConfig,
    proxy: Option<&ProxyConfig>,
    secrets: &dyn SecretResolver,
) -> Result<String> {
    let work = async {
        let be = expandrv_backends::connect(backend, proxy, secrets).await?;
        let n = be.list(&RemotePath::root()).await?.len();
        Ok::<_, expandrv_core::Error>(format!("연결 성공: 루트 항목 {n}개"))
    };
    match tokio::time::timeout(CHECK_TIMEOUT, work).await {
        Ok(r) => r.map_err(|e| e.to_string()),
        Err(_) => Err("연결 확인 시간이 초과되었습니다".into()),
    }
}

/// 저장하기 전에 입력한 연결 정보로 접속과 루트 목록 조회를 확인합니다. 비밀 값은 저장하지 않습니다.
#[tauri::command]
pub async fn test_connection(backend: BackendInput, proxy: Option<ProxyInput>) -> Result<String> {
    // 대상 이름은 메모리 안에서 참조를 잇는 데만 씁니다.
    const ID: &str = "connection-test";
    let mut pending = Vec::new();
    let backend = drives::build_backend(ID, backend, &mut pending).map_err(|e| e.to_string())?;
    let proxy = drives::build_proxy(ID, proxy, None, &mut pending).map_err(|e| e.to_string())?;
    probe(
        &backend,
        proxy.as_ref(),
        &drives::TypedSecrets::new(pending),
    )
    .await
}

/// 저장한 설정과 비밀 값으로 연결을 확인합니다(CLI `expandrv check`와 같음).
#[tauri::command]
pub async fn check_drive(state: State<'_, AppState>, id: String) -> Result<String> {
    let cfg = state.load()?;
    let d = cfg
        .drive(&id)
        .ok_or_else(|| format!("드라이브 {id}가 없습니다"))?;
    probe(&d.backend, d.proxy.as_ref(), &Secrets).await
}

/// 쓰이지 않는 드라이브 문자(Z부터). 설정에 이미 잡힌 문자는 뺍니다.
#[tauri::command]
pub async fn free_drive_letters(state: State<'_, AppState>) -> Result<Vec<String>> {
    let used = unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() };
    let cfg = state.load().unwrap_or_default();
    Ok(free_letters(used, &cfg))
}

pub fn free_letters(used_mask: u32, cfg: &Config) -> Vec<String> {
    (b'D'..=b'Z')
        .rev()
        .filter(|l| used_mask & (1 << (l - b'A')) == 0)
        .map(|l| format!("{}:", l as char))
        .filter(|mp| {
            !cfg.drives
                .iter()
                .any(|d| d.mount_point.eq_ignore_ascii_case(mp))
        })
        .collect()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    pub autostart: bool,
    pub autostart_command: Option<String>,
    pub config_path: String,
    pub log_dir: String,
    pub cli_path: String,
    pub cli_found: bool,
    pub gui_version: &'static str,
    pub agent_running: bool,
    pub agent_version: Option<String>,
}

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> Result<SettingsView> {
    settings_view(&state).await
}

async fn settings_view(state: &AppState) -> Result<SettingsView> {
    let p = &state.paths;
    let command =
        expandrv_autostart::status(expandrv_autostart::RUN_KEY).map_err(|e| e.to_string())?;
    let link = agent::connect(&p.pipe).await?;
    Ok(SettingsView {
        autostart: command.is_some(),
        autostart_command: command,
        config_path: p.config.display().to_string(),
        log_dir: p.log_dir.display().to_string(),
        cli_path: p.cli.display().to_string(),
        cli_found: p.cli.is_file(),
        gui_version: env!("CARGO_PKG_VERSION"),
        agent_running: link.is_some(),
        agent_version: link.map(|l| l.version),
    })
}

/// 로그인 자동 실행을 켜거나 끕니다. 등록 명령은 CLI 에이전트를 가리킵니다(`expandrv autostart enable`과 같음).
#[tauri::command]
pub async fn set_autostart(state: State<'_, AppState>, enabled: bool) -> Result<SettingsView> {
    use expandrv_autostart::{RUN_KEY, command_line, disable, enable};
    let p = &state.paths;
    let r = if enabled {
        if !p.cli.is_file() {
            return Err(format!("CLI 실행 파일이 없습니다: {}", p.cli.display()));
        }
        let exe = std::path::absolute(&p.cli).map_err(|e| e.to_string())?;
        let cfg = std::path::absolute(&p.config).map_err(|e| e.to_string())?;
        enable(RUN_KEY, &command_line(&exe, &cfg))
    } else {
        disable(RUN_KEY)
    };
    r.map_err(|e| e.to_string())?;
    settings_view(&state).await
}

/// 모든 드라이브를 해제하고 에이전트를 끝냅니다. 실행 중이 아니었으면 false입니다.
#[tauri::command]
pub async fn stop_agent(state: State<'_, AppState>) -> Result<bool> {
    agent::stop(&state.paths.pipe).await
}
