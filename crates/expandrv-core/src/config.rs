//! `config.toml` 설정 모델. 비밀 값은 저장하지 않고 [`SecretSource`] 참조만 둡니다.

use crate::{Error, Result};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, rename = "drive")]
    pub drives: Vec<DriveConfig>,
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
    pub backend: BackendConfig,
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
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            metadata_ttl_secs: 30,
            block_size_mib: 4,
            memory_limit_mib: 64,
            poll_interval_secs: 60,
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
}

/// [`SecretSource`]를 실제 값으로 바꿉니다. 3단계에서 DPAPI와 Credential Manager 구현이 추가됩니다.
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

impl Config {
    /// 파일이 없으면 빈 설정을 돌려줍니다.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e.into()),
        };
        let cfg: Config =
            toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        cfg.validate()?;
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
        let cfg: Config = toml::from_str(SAMPLE).unwrap();
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
    fn missing_file_is_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Config::load(&dir.path().join("none.toml")).unwrap(),
            Config::default()
        );
    }
}
