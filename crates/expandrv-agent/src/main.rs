//! `expandrv` CLI: 드라이브 설정 관리와 포그라운드 마운트

mod signal;

use clap::{Args, Parser, Subcommand};
use expandrv_core::config::{
    BackendConfig, CacheConfig, Config, DriveConfig, EnvSecrets, FtpConfig, FtpTls, S3Config,
    SecretSource, SftpConfig, TransferConfig, WebDavConfig,
};
use expandrv_core::{Error, Result};
use expandrv_vfs::{Vfs, VfsOptions};
use expandrv_winfsp::{MountOptions, MountedDrive};
use signal::UnmountSignal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// 마운트 해제 때 남은 업로드를 기다리는 최대 시간
const UPLOAD_DRAIN_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Parser)]
#[command(
    name = "expandrv",
    version,
    about = "원격 스토리지를 Windows 드라이브로 마운트합니다"
)]
struct Cli {
    /// 설정 파일 경로(기본: %APPDATA%\ExpanDrv\config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
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
    /// 드라이브를 마운트하고 Ctrl+C 또는 unmount 요청까지 유지합니다
    Mount {
        /// 마운트할 드라이브 id(생략하면 전체)
        ids: Vec<String>,
    },
    /// 실행 중인 마운트를 해제합니다
    Unmount { id: String },
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
        #[arg(long)]
        password_env: Option<String>,
        /// 고정할 서버 인증서 SHA-256 지문
        #[arg(long)]
        pin_sha256: Option<String>,
    },
    Sftp {
        host: String,
        #[arg(long)]
        user: String,
        #[arg(long, default_value_t = 22)]
        port: u16,
        #[arg(long)]
        password_env: Option<String>,
        #[arg(long)]
        key: Option<PathBuf>,
        #[arg(long)]
        key_passphrase_env: Option<String>,
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
        #[arg(long)]
        secret_env: String,
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
        #[arg(long)]
        password_env: Option<String>,
        /// none, explicit, implicit
        #[arg(long, default_value = "explicit")]
        tls: String,
        #[arg(long, default_value = "utf-8")]
        encoding: String,
        #[arg(long, default_value = "/")]
        root: String,
    },
}

fn env_secret(var: Option<String>) -> Option<SecretSource> {
    var.map(|var| SecretSource::Env { var })
}

fn backend_config(b: AddBackend) -> Result<BackendConfig> {
    Ok(match b {
        AddBackend::Local { root } => BackendConfig::Local {
            root: std::path::absolute(&root)?,
        },
        AddBackend::Webdav {
            url,
            user,
            password_env,
            pin_sha256,
        } => BackendConfig::Webdav(WebDavConfig {
            url,
            username: user,
            password: env_secret(password_env),
            pinned_cert_sha256: pin_sha256,
        }),
        AddBackend::Sftp {
            host,
            user,
            port,
            password_env,
            key,
            key_passphrase_env,
            root,
            known_hosts,
        } => BackendConfig::Sftp(SftpConfig {
            host,
            port,
            username: user,
            password: env_secret(password_env),
            private_key: key,
            key_passphrase: env_secret(key_passphrase_env),
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
            virtual_host,
        } => BackendConfig::S3(S3Config {
            endpoint,
            region,
            bucket,
            prefix,
            access_key_id,
            secret_access_key: SecretSource::Env { var: secret_env },
            path_style: !virtual_host,
        }),
        AddBackend::Ftp {
            host,
            port,
            user,
            password_env,
            tls,
            encoding,
            root,
        } => BackendConfig::Ftp(FtpConfig {
            host,
            port,
            username: user,
            password: env_secret(password_env),
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
    let appdata = std::env::var_os("APPDATA")
        .ok_or_else(|| Error::Config("APPDATA 환경 변수가 없습니다".into()))?;
    Ok(PathBuf::from(appdata).join("ExpanDrv").join("config.toml"))
}

/// 드라이브별 캐시 폴더(`%LOCALAPPDATA%\ExpanDrv\cache\<id>`). 스테이징과 메타 DB를 둡니다.
fn cache_dir(id: &str) -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| Error::Config("LOCALAPPDATA 환경 변수가 없습니다".into()))?;
    Ok(PathBuf::from(base).join("ExpanDrv").join("cache").join(id))
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
    cfg.drives.push(DriveConfig {
        label: a.label.unwrap_or_else(|| a.id.clone()),
        id: a.id,
        mount_point,
        read_only: a.read_only,
        cache: CacheConfig::default(),
        transfer: TransferConfig::default(),
        auto_mount: false,
        proxy: None,
        backend: backend_config(a.backend)?,
    });
    cfg.save(cfg_path)?;
    println!("추가했습니다: {}", cfg_path.display());
    Ok(())
}

fn list(cfg: &Config) {
    if cfg.drives.is_empty() {
        println!("설정된 드라이브가 없습니다.");
        return;
    }
    println!(
        "{:<16} {:<8} {:<8} {:<10} 레이블",
        "ID", "지점", "종류", "상태"
    );
    for d in &cfg.drives {
        let state = if signal::is_mounted(&d.id) {
            "마운트됨"
        } else {
            "-"
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
}

fn mount(cfg: &Config, ids: &[String]) -> Result<()> {
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

    let rt = tokio::runtime::Runtime::new()?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let mut mounted = Vec::new();
    for d in drives {
        let signal = Arc::new(UnmountSignal::create(&d.id)?);
        let backend = expandrv_vfs::throttle(
            rt.block_on(expandrv_backends::connect(
                &d.backend,
                d.proxy.as_ref(),
                &EnvSecrets,
            ))?,
            d.transfer.upload_kib_per_sec * 1024,
            d.transfer.download_kib_per_sec * 1024,
        );
        let vfs = Vfs::new(
            backend,
            rt.handle().clone(),
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
        println!("{} -> {} ({})", d.id, d.mount_point, d.backend.kind());
        let (waiter, tx, id) = (signal.clone(), tx.clone(), d.id.clone());
        std::thread::spawn(move || {
            if waiter.wait() {
                let _ = tx.send(id);
            }
        });
        mounted.push((d.id.clone(), signal, drive, vfs));
    }
    println!("Ctrl+C 또는 `expandrv unmount <id>`로 해제합니다.");

    while !mounted.is_empty() {
        let next = rt.block_on(async {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => None,
                id = rx.recv() => id,
            }
        });
        let leaving: Vec<_> = match next {
            Some(id) => {
                let (out, keep) = mounted.into_iter().partition(|m| m.0 == id);
                mounted = keep;
                out
            }
            None => std::mem::take(&mut mounted),
        };
        for (id, signal, drive, vfs) in leaving {
            signal.set();
            // 새 쓰기를 막은 뒤 남은 업로드를 마칩니다.
            drop(drive);
            let pending = vfs.pending_uploads();
            if pending > 0 {
                println!("{id}: 남은 업로드 {pending}개를 마치는 중입니다");
            }
            match vfs.sync(UPLOAD_DRAIN_TIMEOUT) {
                Ok(()) => println!("해제했습니다: {id}"),
                Err(e) => eprintln!("{id}: {e}"),
            }
        }
    }
    Ok(())
}

fn run(cli: Cli) -> Result<()> {
    let cfg_path = match cli.config {
        Some(p) => p,
        None => default_config_path()?,
    };
    match cli.command {
        Command::Add(a) => add(&cfg_path, *a),
        Command::Remove { id } => {
            let mut cfg = Config::load(&cfg_path)?;
            if signal::is_mounted(&id) {
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
            println!("삭제했습니다: {id}");
            Ok(())
        }
        Command::List => {
            list(&Config::load(&cfg_path)?);
            Ok(())
        }
        Command::Check { id } => {
            let cfg = Config::load(&cfg_path)?;
            let d = cfg
                .drive(&id)
                .ok_or_else(|| Error::Config(format!("드라이브 {id}가 없습니다")))?;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let be =
                    expandrv_backends::connect(&d.backend, d.proxy.as_ref(), &EnvSecrets).await?;
                let n = be.list(&expandrv_core::RemotePath::root()).await?.len();
                println!("연결 성공: {} 루트 항목 {n}개", be.kind());
                Ok(())
            })
        }
        Command::Mount { ids } => mount(&Config::load(&cfg_path)?, &ids),
        Command::Unmount { id } => {
            signal::request_unmount(&id)?;
            println!("해제를 요청했습니다: {id}");
            Ok(())
        }
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("EXPANDRV_LOG")
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    if let Err(e) = run(Cli::parse()) {
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
        let BackendConfig::Webdav(w) = backend_config(a.backend).unwrap() else {
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
        assert!(matches!(backend_config(a.backend).unwrap(), BackendConfig::S3(s) if s.path_style));
    }

    #[test]
    fn add_list_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let run_cli = |args: &[&str]| {
            let mut v = vec!["expandrv", "--config", cfg_path.to_str().unwrap()];
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
}
