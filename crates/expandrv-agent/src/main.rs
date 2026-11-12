//! `expandrv` CLI: 드라이브 설정 관리와 상주 에이전트 제어

mod agent;
mod auto_lock;
mod control;
mod tray;
mod updater;

use clap::{Args, Parser, Subcommand};
use control::{AgentLink, expect_ok};
use expandrv_autostart as autostart;
use expandrv_core::config::{
    BackendConfig, CacheConfig, Config, DriveConfig, FtpConfig, FtpTls, ProxyConfig, S3Config,
    SecretSource, SftpConfig, TransferConfig, WebDavConfig,
};
use expandrv_core::{Error, Result};
use expandrv_ipc::{DriveState, ErrorCode, Request, Response};
use expandrv_secrets::{CredentialStore, Secrets, target_for};
use secrecy::SecretString;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 파이프 이름 재지정(테스트용). `--pipe`가 우선합니다.
const PIPE_ENV: &str = "EXPANDRV_PIPE";

#[derive(Parser)]
#[command(
    name = "expandrv",
    version,
    about = "원격 스토리지를 Windows 드라이브로 마운트합니다"
)]
struct Cli {
    /// 설정 파일 경로(기본: ~/.config/ExpanDrv/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// 에이전트 파이프 이름(테스트용)
    #[arg(long, global = true, hide = true)]
    pipe: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 드라이브 설정을 추가합니다
    Add(Box<AddArgs>),
    /// 드라이브 설정을 삭제합니다
    Remove { id: String },
    /// 설정된 드라이브와 마운트 상태를 보여 줍니다
    List,
    /// 연결과 인증만 확인합니다
    Check { id: String },
    /// 드라이브를 마운트합니다. 에이전트가 없으면 백그라운드로 시작합니다
    Mount {
        /// 마운트할 드라이브 id(생략하면 전체)
        ids: Vec<String>,
    },
    /// 마운트를 해제합니다(남은 업로드를 마칠 때까지 기다림)
    Unmount { id: String },
    /// 상주 에이전트를 포그라운드로 실행합니다
    Agent {
        /// 분리 실행용: 로그를 파일로 남깁니다
        #[arg(long, hide = true)]
        background: bool,
        /// 시작하자마자 auto_mount 드라이브를 마운트합니다
        #[arg(long)]
        autostart: bool,
    },
    /// 모든 드라이브를 해제하고 에이전트를 종료합니다
    Stop,
    /// 전체 드라이브를 해제하고 잠급니다
    Lock,
    /// PIN을 입력해 잠금을 해제합니다
    Unlock,
    /// 잠금 PIN을 등록하거나 바꿉니다
    Pin,
    /// 자격 증명 관리자에 저장한 비밀 값을 관리합니다
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },
    /// 탐색기 우클릭 메뉴를 관리합니다
    Shell {
        #[command(subcommand)]
        action: ShellAction,
    },
    /// Windows 로그인 시 에이전트 자동 실행을 설정합니다
    Autostart {
        #[command(subcommand)]
        action: AutostartAction,
    },
}

#[derive(Subcommand)]
enum SecretAction {
    /// 비밀 값을 입력받아 저장합니다(터미널이 아니면 표준 입력 한 줄)
    Set { id: String, field: String },
    /// 저장한 비밀 값을 지웁니다
    Delete { id: String, field: String },
    /// 저장한 비밀 값 대상 이름을 보여 줍니다(값은 보이지 않음)
    List { id: Option<String> },
}

#[derive(Subcommand)]
enum ShellAction {
    Enable,
    Disable,
    #[command(hide = true)]
    DisableLocal,
    Status,
}

#[derive(Subcommand)]
enum AutostartAction {
    Enable,
    Disable,
    Status,
}

#[derive(Args)]
struct AddArgs {
    id: String,
    /// `X:` 드라이브 문자 또는 존재하지 않는 폴더 경로
    mount_point: String,
    #[arg(long)]
    label: Option<String>,
    #[arg(long)]
    read_only: bool,
    /// 에이전트 자동 실행 때 함께 마운트
    #[arg(long)]
    auto_mount: bool,
    /// socks5://, socks5h:// 또는 http:// 프록시
    #[arg(long)]
    proxy: Option<String>,
    #[arg(long, requires = "proxy")]
    proxy_user: Option<String>,
    #[arg(long, requires = "proxy_user", conflicts_with = "ask_proxy_password")]
    proxy_password_env: Option<String>,
    /// 프록시 암호를 입력받아 자격 증명 관리자에 저장
    #[arg(long, requires = "proxy_user")]
    ask_proxy_password: bool,
    #[command(subcommand)]
    backend: AddBackend,
}

#[derive(Subcommand)]
enum AddBackend {
    Local {
        root: PathBuf,
    },
    Webdav {
        url: String,
        #[arg(long)]
        user: Option<String>,
        /// 암호를 담은 환경 변수 이름
        #[arg(long, conflicts_with = "ask_password")]
        password_env: Option<String>,
        /// 암호를 입력받아 자격 증명 관리자에 저장
        #[arg(long)]
        ask_password: bool,
        /// 고정할 서버 인증서 SHA-256 지문
        #[arg(long)]
        pin_sha256: Option<String>,
    },
    Sftp {
        host: String,
        /// SSH 셸에서 Python 3로 파일 접근(SFTP 서비스 불필요)
        #[arg(long)]
        ssh_shell: bool,
        #[arg(long)]
        user: String,
        #[arg(long, default_value_t = 22)]
        port: u16,
        #[arg(long, conflicts_with = "ask_password")]
        password_env: Option<String>,
        #[arg(long)]
        ask_password: bool,
        #[arg(long)]
        key: Option<PathBuf>,
        #[arg(long, conflicts_with = "ask_key_passphrase")]
        key_passphrase_env: Option<String>,
        #[arg(long, requires = "key")]
        ask_key_passphrase: bool,
        #[arg(long, default_value = "/")]
        root: String,
        #[arg(long)]
        known_hosts: Option<PathBuf>,
    },
    S3 {
        endpoint: String,
        bucket: String,
        #[arg(long, default_value = "us-east-1")]
        region: String,
        #[arg(long, default_value = "")]
        prefix: String,
        #[arg(long)]
        access_key_id: String,
        #[arg(
            long,
            required_unless_present = "ask_secret",
            conflicts_with = "ask_secret"
        )]
        secret_env: Option<String>,
        /// 비밀 키를 입력받아 자격 증명 관리자에 저장
        #[arg(long)]
        ask_secret: bool,
        /// 가상 호스트 방식 주소 사용
        #[arg(long)]
        virtual_host: bool,
    },
    Ftp {
        host: String,
        #[arg(long, default_value_t = 21)]
        port: u16,
        #[arg(long, default_value = "anonymous")]
        user: String,
        #[arg(long, conflicts_with = "ask_password")]
        password_env: Option<String>,
        #[arg(long)]
        ask_password: bool,
        /// none, explicit, implicit
        #[arg(long, default_value = "explicit")]
        tls: String,
        /// 서버 관리자에게 확인한 서버 인증서 SHA-256 지문(TLS 전용)
        #[arg(long)]
        pin_sha256: Option<String>,
        #[arg(long, default_value = "utf-8")]
        encoding: String,
        #[arg(long, default_value = "/")]
        root: String,
    },
}

/// 입력받아 저장할 비밀 값(대상 이름, 안내 문구)
struct Ask {
    target: String,
    prompt: String,
}

/// `--*-env`나 `--ask-*`로 비밀 값 참조를 만듭니다. `ask`면 저장할 목록에 넣습니다.
fn secret_ref(
    id: &str,
    field: &str,
    env: Option<String>,
    ask: bool,
    asks: &mut Vec<Ask>,
) -> Option<SecretSource> {
    if ask {
        let target = target_for(id, field);
        asks.push(Ask {
            target: target.clone(),
            prompt: format!("{id} {field}"),
        });
        Some(SecretSource::Credential { target })
    } else {
        env.map(|var| SecretSource::Env { var })
    }
}

/// 터미널이면 화면에 보이지 않게 입력받고, 아니면 표준 입력 한 줄을 읽습니다.
fn read_secret(prompt: &str) -> Result<SecretString> {
    use std::io::IsTerminal;
    let value = if std::io::stdin().is_terminal() {
        rpassword::prompt_password(format!("{prompt}: "))?
    } else {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let len = line.trim_end_matches(['\r', '\n']).len();
        line.truncate(len);
        line
    };
    if value.is_empty() {
        return Err(Error::Config(format!(
            "{prompt}: 빈 값은 저장하지 않습니다"
        )));
    }
    Ok(SecretString::from(value))
}

fn backend_config(id: &str, b: AddBackend, asks: &mut Vec<Ask>) -> Result<BackendConfig> {
    Ok(match b {
        AddBackend::Local { root } => BackendConfig::Local {
            root: std::path::absolute(&root)?,
        },
        AddBackend::Webdav {
            url,
            user,
            password_env,
            ask_password,
            pin_sha256,
        } => BackendConfig::Webdav(WebDavConfig {
            url,
            username: user,
            password: secret_ref(id, "password", password_env, ask_password, asks),
            pinned_cert_sha256: pin_sha256,
        }),
        AddBackend::Sftp {
            host,
            ssh_shell,
            user,
            port,
            password_env,
            ask_password,
            key,
            key_passphrase_env,
            ask_key_passphrase,
            root,
            known_hosts,
        } => BackendConfig::Sftp(SftpConfig {
            ssh_shell,
            use_ssh_config: false,
            host,
            port,
            username: user,
            password: secret_ref(id, "password", password_env, ask_password, asks),
            private_key: key,
            key_passphrase: secret_ref(
                id,
                "key_passphrase",
                key_passphrase_env,
                ask_key_passphrase,
                asks,
            ),
            root,
            known_hosts,
        }),
        AddBackend::S3 {
            endpoint,
            bucket,
            region,
            prefix,
            access_key_id,
            secret_env,
            ask_secret,
            virtual_host,
        } => BackendConfig::S3(S3Config {
            endpoint,
            region,
            bucket,
            prefix,
            access_key_id,
            secret_access_key: secret_ref(id, "secret_access_key", secret_env, ask_secret, asks)
                .ok_or_else(|| {
                    Error::Config("--secret-env 또는 --ask-secret이 필요합니다".into())
                })?,
            path_style: !virtual_host,
        }),
        AddBackend::Ftp {
            host,
            port,
            user,
            password_env,
            ask_password,
            tls,
            pin_sha256,
            encoding,
            root,
        } => BackendConfig::Ftp(FtpConfig {
            pinned_cert_sha256: pin_sha256,
            host,
            port,
            username: user,
            password: secret_ref(id, "password", password_env, ask_password, asks),
            tls: match tls.as_str() {
                "none" => FtpTls::None,
                "explicit" => FtpTls::Explicit,
                "implicit" => FtpTls::Implicit,
                other => return Err(Error::Config(format!("알 수 없는 TLS 모드 {other:?}"))),
            },
            encoding,
            root,
        }),
    })
}

fn default_config_path() -> Result<PathBuf> {
    expandrv_core::config::default_config_path()
}

/// `%LOCALAPPDATA%\ExpanDrv`
fn data_dir() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| Error::Config("LOCALAPPDATA 환경 변수가 없습니다".into()))?;
    Ok(PathBuf::from(base).join("ExpanDrv"))
}

/// 드라이브별 캐시 폴더(`%LOCALAPPDATA%\ExpanDrv\cache\<id>`). 스테이징과 메타 DB를 둡니다.
fn cache_dir(id: &str) -> Result<PathBuf> {
    Ok(data_dir()?.join("cache").join(id))
}

/// 백그라운드 에이전트 로그 폴더(`%LOCALAPPDATA%\ExpanDrv\logs`)
fn log_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("logs"))
}

fn ipc_err(e: expandrv_ipc::Error) -> Error {
    match e {
        expandrv_ipc::Error::Io(e) => Error::Io(e),
        expandrv_ipc::Error::Timeout => Error::Timeout,
        e => message(e.to_string()),
    }
}

/// 분류 접두어 없이 그대로 보일 오류(에이전트 응답 등)
fn message(msg: impl Into<String>) -> Error {
    Error::Io(std::io::Error::other(msg.into()))
}

fn pipe_name(flag: Option<String>) -> Result<String> {
    match flag.or_else(|| std::env::var(PIPE_ENV).ok()) {
        Some(p) => Ok(p),
        None => expandrv_ipc::default_pipe_name().map_err(ipc_err),
    }
}

/// 드라이브 id로 고정된 볼륨 일련번호(FNV-1a)
fn serial(id: &str) -> u32 {
    id.bytes().fold(0x811c_9dc5u32, |h, b| {
        (h ^ b as u32).wrapping_mul(0x0100_0193)
    })
}

fn valid_mount_point(mp: &str) -> bool {
    let b = mp.as_bytes();
    (b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':') || Path::new(mp).is_absolute()
}

fn add(cfg_path: &Path, a: AddArgs) -> Result<()> {
    let mut cfg = Config::load(cfg_path)?;
    if cfg.drive(&a.id).is_some() {
        return Err(Error::Config(format!("드라이브 {}가 이미 있습니다", a.id)));
    }
    if !valid_mount_point(&a.mount_point) {
        return Err(Error::Config(format!(
            "마운트 지점 {:?}는 X: 또는 절대 경로여야 합니다",
            a.mount_point
        )));
    }
    let mount_point = if a.mount_point.len() == 2 {
        a.mount_point.to_ascii_uppercase()
    } else {
        a.mount_point
    };
    let mut asks = Vec::new();
    let backend = backend_config(&a.id, a.backend, &mut asks)?;
    let proxy = match a.proxy {
        Some(url) => Some(ProxyConfig {
            url,
            password: secret_ref(
                &a.id,
                "proxy_password",
                a.proxy_password_env,
                a.ask_proxy_password,
                &mut asks,
            ),
            username: a.proxy_user,
        }),
        None => None,
    };
    let id = a.id.clone();
    cfg.drives.push(DriveConfig {
        label: a.label.unwrap_or_else(|| a.id.clone()),
        id: a.id,
        mount_point,
        read_only: a.read_only,
        cache: CacheConfig::default(),
        transfer: TransferConfig::default(),
        auto_mount: a.auto_mount,
        network_drive: true,
        proxy,
        backend,
    });
    cfg.validate()?;

    // 비밀 값을 먼저 저장하고, 설정 저장이 실패하면 되돌립니다.
    let store = CredentialStore;
    let mut stored = Vec::new();
    let saved = (|| {
        for ask in &asks {
            store.set(&ask.target, &read_secret(&ask.prompt)?)?;
            stored.push(ask.target.clone());
        }
        cfg.save(cfg_path)
    })();
    if let Err(e) = saved {
        for t in &stored {
            let _ = store.delete(t);
        }
        return Err(e);
    }
    println!("추가했습니다: {} ({id})", cfg_path.display());
    for t in &stored {
        println!("비밀 값을 자격 증명 관리자에 저장했습니다: {t}");
    }
    Ok(())
}

fn list(cfg: &Config, pipe: &str) -> Result<()> {
    if cfg.drives.is_empty() {
        println!("설정된 드라이브가 없습니다.");
        return Ok(());
    }
    let states: HashMap<String, (DriveState, u64, u64)> = match AgentLink::connect(pipe)? {
        Some(mut link) => link
            .status()?
            .drives
            .into_iter()
            .map(|d| (d.id, (d.state, d.pending_uploads, d.active_downloads)))
            .collect(),
        None => HashMap::new(),
    };
    println!(
        "{:<16} {:<8} {:<8} {:<10} 레이블",
        "ID", "지점", "종류", "상태"
    );
    for d in &cfg.drives {
        let state = match states.get(&d.id) {
            Some((DriveState::Mounted, up, down)) => {
                let busy: Vec<String> = [(*up, "업로드"), (*down, "다운로드")]
                    .iter()
                    .filter(|(n, _)| *n > 0)
                    .map(|(n, what)| format!("{what} {n}"))
                    .collect();
                if busy.is_empty() {
                    "마운트됨".to_owned()
                } else {
                    format!("마운트됨({})", busy.join(", "))
                }
            }
            Some((DriveState::Failed(_), _, _)) => "실패".to_owned(),
            Some((DriveState::Unmounted, n, _)) if *n > 0 => format!("해제 중(업로드 {n})"),
            _ => "-".to_owned(),
        };
        println!(
            "{:<16} {:<8} {:<8} {:<10} {}",
            d.id,
            d.mount_point,
            d.backend.kind(),
            state,
            d.label
        );
    }
    for d in &cfg.drives {
        if let Some((DriveState::Failed(msg), _, _)) = states.get(&d.id) {
            println!("{}: {msg}", d.id);
        }
    }
    Ok(())
}

fn mount(cfg_path: &Path, pipe: &str, ids: &[String]) -> Result<()> {
    let cfg = Config::load(cfg_path)?;
    let drives: Vec<&DriveConfig> = if ids.is_empty() {
        cfg.drives.iter().collect()
    } else {
        ids.iter()
            .map(|id| {
                cfg.drive(id)
                    .ok_or_else(|| Error::Config(format!("드라이브 {id}가 없습니다")))
            })
            .collect::<Result<_>>()?
    };
    if drives.is_empty() {
        return Err(Error::Config("마운트할 드라이브가 없습니다".into()));
    }

    let mut link = AgentLink::start(pipe, cfg_path)?;
    // 이미 떠 있던 에이전트도 최신 설정을 쓰게 합니다.
    expect_ok(link.call(&Request::Reload)?)?;
    let mut failed = 0;
    for d in drives {
        match link.call(&Request::Mount { id: d.id.clone() })? {
            Response::Ok => println!("{} -> {} ({})", d.id, d.mount_point, d.backend.kind()),
            Response::Error {
                code: ErrorCode::AlreadyMounted,
                ..
            } => println!("{}: 이미 마운트되어 있습니다", d.id),
            Response::Error { message, .. } => {
                eprintln!("{}: {message}", d.id);
                failed += 1;
            }
            other => return Err(Error::Protocol(format!("예상하지 못한 응답: {other:?}"))),
        }
    }
    if failed > 0 {
        return Err(message(format!(
            "드라이브 {failed}개를 마운트하지 못했습니다"
        )));
    }
    println!("`expandrv unmount <id>` 또는 `expandrv stop`으로 해제합니다.");
    Ok(())
}

fn require_agent(pipe: &str) -> Result<AgentLink> {
    AgentLink::connect(pipe)?.ok_or_else(|| message("에이전트가 실행 중이 아닙니다"))
}

fn unmount(pipe: &str, id: &str) -> Result<()> {
    let mut link = require_agent(pipe)?;
    if let Some(d) = link.status()?.drives.iter().find(|d| d.id == id)
        && d.pending_uploads > 0
    {
        println!(
            "{id}: 남은 업로드 {}개를 마치는 중입니다",
            d.pending_uploads
        );
    }
    expect_ok(link.call(&Request::Unmount { id: id.to_owned() })?)?;
    println!("해제했습니다: {id}");
    Ok(())
}

fn stop(pipe: &str) -> Result<()> {
    let Some(mut link) = AgentLink::connect(pipe)? else {
        println!("에이전트가 실행 중이 아닙니다.");
        return Ok(());
    };
    let pid = link.pid();
    let r = expect_ok(link.call(&Request::Shutdown)?);
    drop(link);
    if !control::wait_process_exit(pid, Duration::from_secs(30)) {
        return Err(message(format!(
            "에이전트(pid {pid})가 아직 종료되지 않았습니다"
        )));
    }
    r?;
    println!("에이전트를 종료했습니다.");
    Ok(())
}

/// 설정을 바꾼 뒤 실행 중인 에이전트가 있으면 다시 읽게 합니다.
fn notify_reload(pipe: &str) {
    if let Ok(Some(mut link)) = AgentLink::connect(pipe)
        && let Err(e) = link.call(&Request::Reload).and_then(expect_ok)
    {
        eprintln!("에이전트 설정 갱신 실패: {e}");
    }
}

fn run(cli: Cli) -> Result<()> {
    let cfg_path = match cli.config {
        Some(p) => expandrv_core::config::expand_tilde(p),
        None => default_config_path()?,
    };
    let pipe = pipe_name(cli.pipe)?;
    match cli.command {
        Command::Add(a) => {
            add(&cfg_path, *a)?;
            notify_reload(&pipe);
            Ok(())
        }
        Command::Remove { id } => {
            let mut cfg = Config::load(&cfg_path)?;
            if let Some(mut link) = AgentLink::connect(&pipe)?
                && link
                    .status()?
                    .drives
                    .iter()
                    .any(|d| d.id == id && d.state == DriveState::Mounted)
            {
                return Err(Error::Config(format!(
                    "{id}가 마운트되어 있습니다. 먼저 해제하세요"
                )));
            }
            let before = cfg.drives.len();
            cfg.drives.retain(|d| d.id != id);
            if cfg.drives.len() == before {
                return Err(Error::Config(format!("드라이브 {id}가 없습니다")));
            }
            cfg.save(&cfg_path)?;
            notify_reload(&pipe);
            println!("삭제했습니다: {id}");
            // 다시 만들 수 없는 암호화 블록 캐시는 지웁니다(업로드 대기 파일은 남김).
            if let Ok(dir) = cache_dir(&id) {
                let _ = std::fs::remove_dir_all(dir.join("blocks"));
            }
            // 이 드라이브용으로 저장한 비밀 값(캐시 키 포함)도 지웁니다.
            let store = CredentialStore;
            for t in store.list(&format!("ExpanDrv/{id}/"))? {
                store.delete(&t)?;
                println!("비밀 값을 지웠습니다: {t}");
            }
            Ok(())
        }
        Command::List => list(&Config::load(&cfg_path)?, &pipe),
        Command::Check { id } => {
            let locked = match AgentLink::connect(&pipe)? {
                Some(mut link) => link.status()?.locked,
                None => expandrv_secrets::lock::PinStore::new(&cfg_path)?.configured()?,
            };
            if locked {
                return Err(message("드라이브가 잠겨 있습니다. 먼저 잠금을 해제하세요"));
            }
            let cfg = Config::load(&cfg_path)?;
            let d = cfg
                .drive(&id)
                .ok_or_else(|| Error::Config(format!("드라이브 {id}가 없습니다")))?;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let be = expandrv_backends::connect(&d.backend, d.proxy.as_ref(), &Secrets).await?;
                let n = be.list(&expandrv_core::RemotePath::root()).await?.len();
                println!("연결 성공: {} 루트 항목 {n}개", be.kind());
                Ok(())
            })
        }
        Command::Mount { ids } => mount(&cfg_path, &pipe, &ids),
        Command::Unmount { id } => unmount(&pipe, &id),
        Command::Agent { autostart, .. } => agent::run(cfg_path, &pipe, autostart),
        Command::Stop => stop(&pipe),
        Command::Lock => {
            let mut link = AgentLink::start(&pipe, &cfg_path)?;
            expect_ok(link.call(&Request::Lock)?)?;
            println!("모든 드라이브를 잠갔습니다");
            Ok(())
        }
        Command::Unlock => {
            use secrecy::ExposeSecret;
            let pin = read_secret("잠금 PIN")?;
            let mut link = AgentLink::start(&pipe, &cfg_path)?;
            expect_ok(link.call(&Request::Unlock {
                pin: expandrv_ipc::Pin(pin.expose_secret().to_owned()),
            })?)?;
            println!("잠금을 해제했습니다. 필요한 드라이브를 마운트하세요");
            Ok(())
        }
        Command::Pin => {
            use secrecy::ExposeSecret;
            let configured = expandrv_secrets::lock::PinStore::new(&cfg_path)?.configured()?;
            let current = if configured {
                read_secret("현재 PIN")?
            } else {
                SecretString::from(String::new())
            };
            let pin = read_secret("새 PIN (숫자 6~12자리)")?;
            let again = read_secret("새 PIN 확인")?;
            if pin.expose_secret() != again.expose_secret() {
                return Err(message("새 PIN 확인이 일치하지 않습니다"));
            }
            let mut link = AgentLink::start(&pipe, &cfg_path)?;
            expect_ok(link.call(&Request::SetPin {
                current: expandrv_ipc::Pin(current.expose_secret().to_owned()),
                new: expandrv_ipc::Pin(pin.expose_secret().to_owned()),
            })?)?;
            println!("PIN을 저장했습니다");
            Ok(())
        }
        Command::Secret { action } => secret(action),
        Command::Autostart { action } => autostart_cmd(action, &cfg_path),
        Command::Shell { action } => {
            match action {
                ShellAction::Enable => {
                    let gui = std::env::current_exe()?.with_file_name("expandrv-gui.exe");
                    expandrv_shell::install(&gui, &cfg_path)?;
                    println!("탐색기 메뉴를 등록했습니다");
                }
                ShellAction::Disable => {
                    expandrv_shell::uninstall()?;
                    println!("탐색기 메뉴를 해제했습니다");
                }
                ShellAction::DisableLocal => {
                    let gui = std::env::current_exe()?.with_file_name("expandrv-gui.exe");
                    expandrv_shell::uninstall_for(&gui)?;
                }
                ShellAction::Status => println!(
                    "{}",
                    if expandrv_shell::enabled()? {
                        "켜짐"
                    } else {
                        "꺼짐"
                    }
                ),
            }
            Ok(())
        }
    }
}

fn secret(action: SecretAction) -> Result<()> {
    let store = CredentialStore;
    match action {
        SecretAction::Set { id, field } => {
            let target = target_for(&id, &field);
            store.set(&target, &read_secret(&format!("{id} {field}"))?)?;
            println!("저장했습니다: {target}");
            println!("설정에서는 {{ from = \"credential\", target = \"{target}\" }}로 참조합니다.");
        }
        SecretAction::Delete { id, field } => {
            let target = target_for(&id, &field);
            store.delete(&target).map_err(|e| match e {
                Error::NotFound => Error::Config(format!("{target}가 없습니다")),
                e => e,
            })?;
            println!("지웠습니다: {target}");
        }
        SecretAction::List { id } => {
            let prefix = match id {
                Some(id) => format!("ExpanDrv/{id}/"),
                None => "ExpanDrv/".to_owned(),
            };
            for t in store.list(&prefix)? {
                println!("{t}");
            }
        }
    }
    Ok(())
}

fn autostart_cmd(action: AutostartAction, cfg_path: &Path) -> Result<()> {
    use autostart::RUN_KEY;
    match action {
        AutostartAction::Enable => {
            let exe = std::env::current_exe()?;
            let cfg_abs = std::path::absolute(cfg_path)?;
            autostart::enable(RUN_KEY, &autostart::command_line(&exe, &cfg_abs))?;
            println!("로그인할 때 에이전트를 자동으로 시작합니다.");
            let cfg = Config::load(cfg_path)?;
            if !cfg.drives.iter().any(|d| d.auto_mount) {
                println!("auto_mount가 켜진 드라이브가 없어 시작만 하고 마운트하지 않습니다.");
            }
        }
        AutostartAction::Disable => {
            autostart::disable(RUN_KEY)?;
            println!("자동 시작을 껐습니다.");
        }
        AutostartAction::Status => match autostart::status(RUN_KEY)? {
            Some(cmd) => println!("켜짐: {cmd}"),
            None => println!("꺼짐"),
        },
    }
    Ok(())
}

/// 로그 출력 설정. 분리 실행한 에이전트는 파일로, 나머지는 stderr로 남깁니다.
fn init_logging(background: bool) {
    use expandrv_log::Redacting;
    use tracing_subscriber::fmt::writer::BoxMakeWriter;
    use tracing_subscriber::prelude::*;

    let file = if background {
        log_dir().ok().and_then(|dir| {
            tracing_appender::rolling::Builder::new()
                .rotation(tracing_appender::rolling::Rotation::DAILY)
                .filename_prefix("agent")
                .filename_suffix("log")
                .max_log_files(7)
                .build(dir)
                .ok()
        })
    } else {
        None
    };
    let to_file = file.is_some();
    // 출력 직전에 비밀 값과 사용자 계정명을 가립니다.
    let (writer, default_level) = match file {
        Some(f) => (BoxMakeWriter::new(Redacting(f)), "info"),
        None => (BoxMakeWriter::new(Redacting(std::io::stderr)), "warn"),
    };
    let filter = tracing_subscriber::EnvFilter::try_from_env("EXPANDRV_LOG")
        .unwrap_or_else(|_| default_level.into());
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(!to_file),
        )
        .with(filter)
        .init();
    if to_file {
        // 콘솔이 없으므로 패닉도 로그에 남깁니다.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            tracing::error!("패닉: {info}");
            prev(info);
        }));
    }
}

fn main() {
    let cli = Cli::parse();
    let background = matches!(
        cli.command,
        Command::Agent {
            background: true,
            ..
        }
    );
    init_logging(background);
    if let Err(e) = run(cli) {
        if background {
            tracing::error!("{e}");
        }
        eprintln!("오류: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_add_variants() {
        let cli = Cli::try_parse_from([
            "expandrv",
            "add",
            "nas",
            "W:",
            "webdav",
            "https://dav.example.com/",
            "--user",
            "u",
            "--password-env",
            "NAS_PW",
        ])
        .unwrap();
        let Command::Add(a) = cli.command else {
            panic!()
        };
        let BackendConfig::Webdav(w) = backend_config(&a.id, a.backend, &mut Vec::new()).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            w.password,
            Some(SecretSource::Env {
                var: "NAS_PW".into()
            })
        );

        let cli = Cli::try_parse_from([
            "expandrv",
            "add",
            "b",
            "S:",
            "s3",
            "https://s3.example.com",
            "bkt",
            "--access-key-id",
            "AK",
            "--secret-env",
            "S3_SECRET",
        ])
        .unwrap();
        let Command::Add(a) = cli.command else {
            panic!()
        };
        assert!(matches!(
            backend_config(&a.id, a.backend, &mut Vec::new()).unwrap(),
            BackendConfig::S3(s) if s.path_style
        ));

        // --ask-* 는 자격 증명 참조를 만들고 입력 목록에 대상을 올립니다.
        let cli = Cli::try_parse_from([
            "expandrv",
            "add",
            "b",
            "S:",
            "s3",
            "https://s3.example.com",
            "bkt",
            "--access-key-id",
            "AK",
            "--ask-secret",
        ])
        .unwrap();
        let Command::Add(a) = cli.command else {
            panic!()
        };
        let mut asks = Vec::new();
        let BackendConfig::S3(s3) = backend_config(&a.id, a.backend, &mut asks).unwrap() else {
            panic!()
        };
        assert_eq!(
            s3.secret_access_key,
            SecretSource::Credential {
                target: "ExpanDrv/b/secret_access_key".into()
            }
        );
        assert_eq!(asks.len(), 1);
        assert!(
            Cli::try_parse_from([
                "expandrv",
                "add",
                "b",
                "S:",
                "s3",
                "https://s3.example.com",
                "bkt",
                "--access-key-id",
                "AK",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "expandrv",
                "add",
                "n",
                "W:",
                "webdav",
                "https://dav.example.com/",
                "--password-env",
                "X",
                "--ask-password",
            ])
            .is_err()
        );
    }

    #[test]
    fn add_list_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        // 실제 에이전트와 섞이지 않도록 없는 파이프 이름을 씁니다.
        let pipe = format!(r"\\.\pipe\expandrv-unit-{}", std::process::id());
        let run_cli = |args: &[&str]| {
            let mut v = vec![
                "expandrv",
                "--config",
                cfg_path.to_str().unwrap(),
                "--pipe",
                &pipe,
            ];
            v.extend_from_slice(args);
            run(Cli::try_parse_from(v).unwrap())
        };
        let root = dir.path().to_str().unwrap();
        run_cli(&["add", "t1", "q:", "local", root]).unwrap();
        assert!(run_cli(&["add", "t1", "R:", "local", root]).is_err());
        assert!(run_cli(&["add", "t2", "bad", "local", root]).is_err());
        let cfg = Config::load(&cfg_path).unwrap();
        assert_eq!(cfg.drives[0].mount_point, "Q:");
        run_cli(&["list"]).unwrap();
        run_cli(&["remove", "t1"]).unwrap();
        assert!(Config::load(&cfg_path).unwrap().drives.is_empty());
        assert!(run_cli(&["unmount", "t1"]).is_err());
    }

    #[test]
    fn serial_is_stable() {
        assert_eq!(serial("nas"), serial("nas"));
        assert_ne!(serial("nas"), serial("nas2"));
    }

    #[test]
    fn default_config_path_points_to_dot_config() {
        let path = default_config_path().unwrap();
        assert!(
            path.ends_with(".config/ExpanDrv/config.toml")
                || path.ends_with(r".config\ExpanDrv\config.toml")
        );
    }
}
