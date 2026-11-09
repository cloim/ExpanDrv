//! `config.toml` 설정 모델. 비밀 값은 저장하지 않고 [`SecretSource`] 참조만 둡니다.

use crate::{Error, Result};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 현재 설정 스키마 버전. 형식이 바뀌면 올리고 [`migrate`]에 변환 단계를 추가합니다.
pub const CONFIG_VERSION: u32 = 3;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// 설정 스키마 버전. 없으면 0(초기 형식)으로 봅니다.
    #[serde(default)]
    pub version: u32,
    #[serde(default, rename = "drive")]
    pub drives: Vec<DriveConfig>,
    #[serde(default)]
    pub security: SecurityConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            drives: Vec::new(),
            security: SecurityConfig::default(),
        }
    }
}

/// PIN 등록 뒤 적용하는 자동 잠금 정책. 유휴 잠금 0은 사용 안 함입니다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityConfig {
    pub lock_on_session_lock: bool,
    pub idle_minutes: u32,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            lock_on_session_lock: true,
            idle_minutes: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriveConfig {
    /// 드라이브 식별자. 캐시 폴더 이름으로도 쓰므로 영문, 숫자, `-`, `_`만 허용합니다.
    pub id: String,
    /// 탐색기에 보일 볼륨 레이블
    pub label: String,
    /// `X:` 형태의 드라이브 문자 또는 빈 폴더 경로
    pub mount_point: String,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub transfer: TransferConfig,
    /// 에이전트가 시작할 때 자동으로 마운트할지 여부
    #[serde(default)]
    pub auto_mount: bool,
    /// 이 드라이브의 원격 연결에 쓸 프록시
    #[serde(default)]
    pub proxy: Option<ProxyConfig>,
    pub backend: BackendConfig,
}

/// SOCKS5 또는 HTTP CONNECT 프록시. 자격 증명은 URL이 아닌 별도 필드로 둡니다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    /// `socks5://host:port`, `socks5h://host:port`(원격 DNS) 또는 `http://host:port`
    pub url: String,
    pub username: Option<String>,
    pub password: Option<SecretSource>,
}

impl ProxyConfig {
    /// URL 형식을 확인하고 (scheme, host, port)를 돌려줍니다.
    pub fn parse(&self) -> Result<(&str, &str, u16)> {
        let bad = |why: &str| Error::Config(format!("프록시 {:?}: {why}", self.url));
        let (scheme, rest) = self
            .url
            .split_once("://")
            .ok_or_else(|| bad("scheme://host:port 형식이 아닙니다"))?;
        if !matches!(scheme, "socks5" | "socks5h" | "http") {
            return Err(bad("socks5, socks5h, http만 지원합니다"));
        }
        let rest = rest.trim_end_matches('/');
        if rest.contains('@') || rest.contains('/') {
            return Err(bad("자격 증명과 경로는 URL에 넣을 수 없습니다"));
        }
        let (host, port) = rest
            .rsplit_once(':')
            .ok_or_else(|| bad("포트가 없습니다"))?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let port = port.parse().map_err(|_| bad("포트가 올바르지 않습니다"))?;
        if host.is_empty() {
            return Err(bad("호스트가 없습니다"));
        }
        Ok((scheme, host, port))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// 디렉터리 목록과 속성 캐시 유지 시간(초)
    pub metadata_ttl_secs: u64,
    /// 블록 캐시 블록 크기(MiB)
    pub block_size_mib: u32,
    /// 블록 캐시 메모리 상한(MiB)
    pub memory_limit_mib: u32,
    /// 원격 변경 확인 주기(초). 0이면 확인하지 않습니다.
    pub poll_interval_secs: u64,
    /// 암호화한 디스크 블록 캐시 상한(MiB). 0이면 디스크 캐시를 쓰지 않습니다.
    pub disk_limit_mib: u64,
    /// 적응형 선행 읽기 상한. 0이면 선행 읽기를 끕니다.
    pub readahead_blocks: u32,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            metadata_ttl_secs: 30,
            block_size_mib: 4,
            memory_limit_mib: 64,
            poll_interval_secs: 60,
            disk_limit_mib: 1024,
            readahead_blocks: 4,
        }
    }
}

/// 드라이브별 전송 속도 제한(KiB/s). 0이면 제한하지 않습니다.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TransferConfig {
    pub upload_kib_per_sec: u64,
    pub download_kib_per_sec: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum BackendConfig {
    Local { root: PathBuf },
    Webdav(WebDavConfig),
    Sftp(SftpConfig),
    S3(S3Config),
    Ftp(FtpConfig),
}

impl BackendConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            BackendConfig::Local { .. } => "local",
            BackendConfig::Webdav(_) => "webdav",
            BackendConfig::Sftp(_) => "sftp",
            BackendConfig::S3(_) => "s3",
            BackendConfig::Ftp(_) => "ftp",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebDavConfig {
    /// `https://host/path/` 형태의 루트 URL
    pub url: String,
    pub username: Option<String>,
    pub password: Option<SecretSource>,
    /// 자체 서명 인증서를 허용할 때 고정할 서버 인증서 SHA-256 지문(16진수)
    pub pinned_cert_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpConfig {
    pub host: String,
    #[serde(default = "default_sftp_port")]
    pub port: u16,
    pub username: String,
    pub password: Option<SecretSource>,
    pub private_key: Option<PathBuf>,
    pub key_passphrase: Option<SecretSource>,
    /// 원격 루트 디렉터리
    #[serde(default = "default_root")]
    pub root: String,
    /// 생략하면 `%USERPROFILE%\.ssh\known_hosts`를 사용합니다.
    pub known_hosts: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3Config {
    /// `https://s3.example.com` 형태의 엔드포인트
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// 버킷 안의 루트 접두사(`photos/` 등). 비우면 버킷 루트입니다.
    #[serde(default)]
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key: SecretSource,
    /// 경로 방식 주소(`endpoint/bucket/key`) 사용 여부. MinIO 등 호환 서버는 대개 true입니다.
    #[serde(default = "default_true")]
    pub path_style: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FtpConfig {
    pub host: String,
    #[serde(default = "default_ftp_port")]
    pub port: u16,
    #[serde(default = "default_ftp_user")]
    pub username: String,
    pub password: Option<SecretSource>,
    #[serde(default)]
    pub tls: FtpTls,
    /// 서버 관리자에게 확인한 리프 인증서 SHA-256 지문. 비우면 시스템 신뢰 저장소로 검증합니다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_cert_sha256: Option<String>,
    /// 서버 파일 이름 문자셋(`utf-8`, `euc-kr` 등 WHATWG 레이블)
    #[serde(default = "default_encoding")]
    pub encoding: String,
    #[serde(default = "default_root")]
    pub root: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FtpTls {
    /// 평문 FTP. 신뢰 구간에서만 사용합니다.
    None,
    /// AUTH TLS(FTPES)
    #[default]
    Explicit,
    /// 990 포트 암묵적 TLS
    Implicit,
}

/// 비밀 값 참조. 설정 파일에는 값 자체를 두지 않습니다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "lowercase", deny_unknown_fields)]
pub enum SecretSource {
    /// 환경 변수에서 읽습니다.
    Env { var: String },
    /// Windows 자격 증명 관리자에 DPAPI(현재 사용자)로 암호화해 저장한 값입니다.
    Credential { target: String },
}

/// [`SecretSource`]를 실제 값으로 바꿉니다.
pub trait SecretResolver: Send + Sync {
    fn resolve(&self, source: &SecretSource) -> Result<SecretString>;
}

/// 환경 변수 기반 기본 해석기
pub struct EnvSecrets;

impl SecretResolver for EnvSecrets {
    fn resolve(&self, source: &SecretSource) -> Result<SecretString> {
        match source {
            SecretSource::Env { var } => std::env::var(var)
                .map(SecretString::from)
                .map_err(|_| Error::Config(format!("환경 변수 {var}가 없습니다"))),
            SecretSource::Credential { .. } => {
                Err(Error::Unsupported("자격 증명 저장소 해석기가 필요합니다"))
            }
        }
    }
}

fn default_sftp_port() -> u16 {
    22
}
fn default_ftp_port() -> u16 {
    21
}
fn default_ftp_user() -> String {
    "anonymous".to_owned()
}
fn default_root() -> String {
    "/".to_owned()
}
fn default_encoding() -> String {
    "utf-8".to_owned()
}
fn default_true() -> bool {
    true
}

/// 이전 스키마의 설정 표를 현재 형식으로 바꿉니다. 바꾼 경우 이전 버전을 돌려줍니다.
pub fn migrate(table: &mut toml::Table) -> Result<Option<u32>> {
    let from = match table.get("version") {
        None => 0,
        Some(v) => v
            .as_integer()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| Error::Config("version이 올바르지 않습니다".into()))?,
    };
    if from > CONFIG_VERSION {
        return Err(Error::Config(format!(
            "설정 버전 {from}은 이 프로그램({CONFIG_VERSION})보다 새롭습니다"
        )));
    }
    if from == CONFIG_VERSION {
        return Ok(None);
    }
    // v0 -> v1: 버전 필드. v1 -> v2: 자동 잠금. v2 -> v3: 적응형 선행 읽기 상한.
    table.insert(
        "version".into(),
        toml::Value::Integer(CONFIG_VERSION.into()),
    );
    Ok(Some(from))
}

impl Config {
    /// 파일이 없으면 빈 설정을 돌려줍니다. 이전 형식이면 `.v<버전>.bak`으로 백업하고 새 형식으로 저장합니다.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        let parse_err =
            |e: &dyn std::fmt::Display| Error::Config(format!("{}: {e}", path.display()));
        let mut table: toml::Table = toml::from_str(&text).map_err(|e| parse_err(&e))?;
        let migrated = migrate(&mut table)?;
        let cfg: Config = table.try_into().map_err(|e| parse_err(&e))?;
        cfg.validate()?;
        if let Some(from) = migrated {
            use std::io::Write;
            let backup = path.with_extension(format!("toml.v{from}.bak"));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&backup)
            {
                Ok(mut file) => {
                    if let Err(error) = file.write_all(text.as_bytes()) {
                        drop(file);
                        let _ = std::fs::remove_file(&backup);
                        return Err(error.into());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            cfg.save(path)?;
        }
        Ok(cfg)
    }

    /// 임시 파일에 쓴 뒤 교체해 부분 기록을 막습니다.
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let text = toml::to_string_pretty(self).map_err(|e| Error::Config(e.to_string()))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.security.idle_minutes > 1440 {
            return Err(Error::Config(
                "유휴 잠금 시간은 0~1440분으로 설정하세요".into(),
            ));
        }
        for d in &self.drives {
            if d.cache.readahead_blocks > 16 {
                return Err(Error::Config("선행 읽기 상한은 0~16개로 설정하세요".into()));
            }
            if let Some(p) = &d.proxy {
                p.parse()?;
            }
        }
        let mut ids = std::collections::HashSet::new();
        let mut mounts = std::collections::HashSet::new();
        for d in &self.drives {
            if d.id.is_empty()
                || !d
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(Error::Config(format!(
                    "드라이브 id {:?}가 올바르지 않습니다",
                    d.id
                )));
            }
            if !ids.insert(d.id.as_str()) {
                return Err(Error::Config(format!(
                    "드라이브 id {:?}가 중복됩니다",
                    d.id
                )));
            }
            if !mounts.insert(d.mount_point.to_ascii_uppercase()) {
                return Err(Error::Config(format!(
                    "마운트 지점 {:?}가 중복됩니다",
                    d.mount_point
                )));
            }
        }
        Ok(())
    }

    pub fn drive(&self, id: &str) -> Option<&DriveConfig> {
        self.drives.iter().find(|d| d.id == id)
    }
}

/// 현재 사용자의 홈 디렉터리를 구합니다(`USERPROFILE` 또는 `HOME`).
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// `~` 접두사로 시작하는 경로를 홈 디렉터리 경로로 확장합니다.
pub fn expand_tilde(path: impl AsRef<Path>) -> PathBuf {
    let p = path.as_ref();
    if let Ok(rest) = p.strip_prefix("~")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    p.to_path_buf()
}

/// 기본 설정 디렉터리 경로(`~/.config/ExpanDrv`).
pub fn default_config_dir() -> Result<PathBuf> {
    let home = home_dir().ok_or_else(|| {
        Error::Config("홈 디렉터리(USERPROFILE 또는 HOME)를 찾을 수 없습니다".into())
    })?;
    Ok(home.join(".config").join("ExpanDrv"))
}

/// 이전 버전에서 쓰던 설정 파일 경로(`%APPDATA%\ExpanDrv\config.toml`).
pub fn legacy_config_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("ExpanDrv").join("config.toml"))
}

/// 기본 설정 파일 경로(`~/.config/ExpanDrv/config.toml`).
/// 만약 새 위치에 설정 파일이 없고 기존 `%APPDATA%\ExpanDrv\config.toml`이 있으면
/// 새 위치로 안전하게 복사(이관)합니다.
pub fn default_config_path() -> Result<PathBuf> {
    let target = default_config_dir()?.join("config.toml");
    if !target.exists()
        && let Some(legacy) = legacy_config_path()
        && legacy.exists()
    {
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::copy(&legacy, &target);
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[[drive]]
id = "nas"
label = "NAS"
mount_point = "W:"
backend = { type = "webdav", url = "https://dav.example.com/", username = "u", password = { from = "env", var = "NAS_PW" } }

[[drive]]
id = "scratch"
label = "Scratch"
mount_point = "X:"
read_only = true
[drive.backend]
type = "local"
root = 'C:\tmp'
"#;

    #[test]
    fn parses_and_roundtrips() {
        let mut cfg: Config = toml::from_str(SAMPLE).unwrap();
        assert_eq!(cfg.version, 0);
        cfg.version = CONFIG_VERSION;
        cfg.validate().unwrap();
        assert_eq!(cfg.drives.len(), 2);
        assert_eq!(cfg.drives[0].backend.kind(), "webdav");
        assert_eq!(cfg.drives[1].cache, CacheConfig::default());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg/config.toml");
        cfg.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), cfg);
        assert!(!std::fs::read_to_string(&path).unwrap().contains("NAS_PW=")); // 값이 아닌 참조만 기록
    }

    #[test]
    fn rejects_duplicates_and_bad_ids() {
        let mut cfg: Config = toml::from_str(SAMPLE).unwrap();
        cfg.drives[1].mount_point = "w:".into();
        assert!(cfg.validate().is_err());
        cfg.drives[1].mount_point = "X:".into();
        cfg.drives[1].id = "../x".into();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_inline_password() {
        let bad = SAMPLE.replace(
            r#"password = { from = "env", var = "NAS_PW" }"#,
            r#"password = "plain""#,
        );
        assert!(toml::from_str::<Config>(&bad).is_err());
    }

    #[test]
    fn migrates_unversioned_file_with_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, SAMPLE).unwrap();
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.version, CONFIG_VERSION);
        assert_eq!(
            std::fs::read_to_string(path.with_extension("toml.v0.bak")).unwrap(),
            SAMPLE
        );
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains(&format!("version = {CONFIG_VERSION}"))
        );
        // 두 번째 불러오기에서는 다시 백업하지 않습니다.
        std::fs::remove_file(path.with_extension("toml.v0.bak")).unwrap();
        Config::load(&path).unwrap();
        assert!(!path.with_extension("toml.v0.bak").exists());
    }

    #[test]
    fn rejects_newer_config_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 99\n").unwrap();
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn proxy_url_validation() {
        let p = |url: &str| ProxyConfig {
            url: url.into(),
            username: None,
            password: None,
        };
        assert_eq!(
            p("socks5://127.0.0.1:1080").parse().unwrap(),
            ("socks5", "127.0.0.1", 1080)
        );
        assert_eq!(
            p("http://proxy.example.com:3128/").parse().unwrap().1,
            "proxy.example.com"
        );
        assert_eq!(p("socks5h://[::1]:1080").parse().unwrap().1, "::1");
        for bad in [
            "ftp://h:1",
            "socks5://u:p@h:1",
            "http://h",
            "http://h:x",
            "h:1",
        ] {
            assert!(p(bad).parse().is_err(), "{bad}");
        }
    }

    #[test]
    fn credential_source_roundtrips() {
        let s: SecretSource =
            toml::from_str("from = \"credential\"\ntarget = \"ExpanDrv/nas/password\"").unwrap();
        assert_eq!(
            s,
            SecretSource::Credential {
                target: "ExpanDrv/nas/password".into()
            }
        );
        assert!(EnvSecrets.resolve(&s).is_err());
    }

    #[test]
    fn missing_file_is_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Config::load(&dir.path().join("none.toml")).unwrap(),
            Config::default()
        );
    }
    #[test]
    fn migrates_v1_security_policy_without_losing_drives() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let previous = format!("version = 1\n{SAMPLE}");
        std::fs::write(&path, &previous).unwrap();
        let mut config = Config::load(&path).unwrap();
        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(config.security, SecurityConfig::default());
        assert_eq!(
            std::fs::read_to_string(path.with_extension("toml.v1.bak")).unwrap(),
            previous
        );
        assert!(!config.drives.is_empty());
        config.security.idle_minutes = 15;
        config.security.lock_on_session_lock = false;
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap().security, config.security);
        config.security.idle_minutes = 1441;
        assert!(config.save(&path).is_err());
        assert_eq!(Config::load(&path).unwrap().security.idle_minutes, 15);
    }
    #[test]
    fn migration_keeps_an_existing_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let backup = path.with_extension("toml.v1.bak");
        std::fs::write(&path, "version = 1\n").unwrap();
        std::fs::write(&backup, "original backup").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.version, CONFIG_VERSION);
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "original backup");
    }

    #[test]
    fn migrates_v2_readahead_with_backup_and_validates_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let before = format!("version = 2\n{SAMPLE}");
        std::fs::write(&path, &before).unwrap();
        let mut cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.version, 3);
        assert_eq!(cfg.drives[0].cache.readahead_blocks, 4);
        assert_eq!(
            std::fs::read_to_string(path.with_extension("toml.v2.bak")).unwrap(),
            before
        );
        cfg.drives[0].cache.readahead_blocks = 17;
        assert!(cfg.save(&path).is_err());
        cfg.drives[0].cache.readahead_blocks = 0;
        cfg.save(&path).unwrap();
        assert_eq!(
            Config::load(&path).unwrap().drives[0]
                .cache
                .readahead_blocks,
            0
        );
    }

    #[test]
    fn default_config_path_ends_with_dot_config() {
        let path = default_config_path().unwrap();
        assert!(
            path.ends_with(".config/ExpanDrv/config.toml")
                || path.ends_with(r".config\ExpanDrv\config.toml")
        );
    }

    #[test]
    fn expand_tilde_replaces_home_prefix() {
        let home = home_dir().unwrap();
        let expanded = expand_tilde("~/.config/ExpanDrv");
        assert_eq!(expanded, home.join(".config").join("ExpanDrv"));
        let not_tilde = expand_tilde(r"C:\tmp\test");
        assert_eq!(not_tilde, PathBuf::from(r"C:\tmp\test"));
    }
}
