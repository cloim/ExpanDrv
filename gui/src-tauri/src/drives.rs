//! 드라이브 설정 입력 검증, 설정 파일 반영, 비밀 값 저장. 화면 입력은 여기서 모두 다시 검사합니다.

use expandrv_core::config::{
    BackendConfig, CacheConfig, Config, DriveConfig, FtpConfig, FtpTls, ProxyConfig, S3Config,
    SecretResolver, SecretSource, SftpConfig, TransferConfig, WebDavConfig,
};
use expandrv_core::{Error, Result};
use expandrv_secrets::{CredentialStore, MAX_SECRET_BYTES, target_for};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 화면에서 받은 비밀 값. 직렬화하지 않으므로 응답이나 설정으로 되돌아갈 수 없습니다.
pub struct Secret(SecretString);

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(d).map(|s| Secret(SecretString::from(s)))
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl Secret {
    /// 빈 값은 입력하지 않은 것으로 봅니다.
    fn provided(&self) -> bool {
        !self.0.expose_secret().is_empty()
    }
}

/// 비밀 값 저장소. 실제로는 자격 증명 관리자이고, 테스트에서는 메모리 저장소로 바꿉니다.
pub trait SecretStore {
    fn set(&self, target: &str, value: &SecretString) -> Result<()>;
    fn delete(&self, target: &str) -> Result<()>;
    fn list(&self, prefix: &str) -> Result<Vec<String>>;
}

impl SecretStore for CredentialStore {
    fn set(&self, target: &str, value: &SecretString) -> Result<()> {
        CredentialStore::set(self, target, value)
    }
    fn delete(&self, target: &str) -> Result<()> {
        CredentialStore::delete(self, target)
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        CredentialStore::list(self, prefix)
    }
}

/// 설정에 참조로 넣고 저장소에 기록할 비밀 값
pub struct PendingSecret {
    pub target: String,
    pub value: SecretString,
}

/// 아직 저장하지 않은 입력 비밀 값으로 참조를 해석합니다(연결 확인용).
pub struct TypedSecrets(HashMap<String, SecretString>);

impl TypedSecrets {
    pub fn new(pending: Vec<PendingSecret>) -> Self {
        Self(pending.into_iter().map(|p| (p.target, p.value)).collect())
    }
}

impl SecretResolver for TypedSecrets {
    fn resolve(&self, source: &SecretSource) -> Result<SecretString> {
        match source {
            SecretSource::Credential { target } => self
                .0
                .get(target)
                .cloned()
                .ok_or_else(|| Error::Config(format!("{target} 값이 입력되지 않았습니다"))),
            SecretSource::Env { var } => Err(Error::Config(format!(
                "환경 변수 {var} 참조는 화면에서 확인하지 않습니다"
            ))),
        }
    }
}

fn bad(msg: impl Into<String>) -> Error {
    Error::Config(msg.into())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewDrive {
    pub id: String,
    pub label: String,
    pub mount_point: String,
    pub read_only: bool,
    pub auto_mount: bool,
    pub proxy: Option<ProxyInput>,
    pub backend: BackendInput,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProxyInput {
    pub url: String,
    pub username: Option<String>,
    /// 비우면 속성 화면에서는 기존 저장 값을 유지합니다.
    pub password: Option<Secret>,
}

#[derive(Debug, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "lowercase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum BackendInput {
    Local {
        root: String,
    },
    Webdav {
        url: String,
        username: Option<String>,
        password: Option<Secret>,
        pinned_cert_sha256: Option<String>,
    },
    Sftp {
        host: String,
        port: u16,
        username: String,
        password: Option<Secret>,
        private_key: Option<String>,
        key_passphrase: Option<Secret>,
        root: String,
        known_hosts: Option<String>,
    },
    S3 {
        endpoint: String,
        region: String,
        bucket: String,
        prefix: String,
        access_key_id: String,
        secret_access_key: Secret,
        path_style: bool,
    },
    Ftp {
        host: String,
        port: u16,
        username: String,
        password: Option<Secret>,
        tls: FtpTls,
        encoding: String,
        root: String,
    },
}

/// 드라이브 id: 캐시 폴더와 자격 증명 대상 이름에 쓰므로 영문, 숫자, `-`, `_`만 받습니다.
pub fn check_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 32
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(bad("드라이브 id는 영문, 숫자, -, _로 1~32자여야 합니다"));
    }
    Ok(())
}

fn check_text(what: &str, value: &str, max: usize) -> Result<()> {
    if value.chars().count() > max {
        return Err(bad(format!("{what}은(는) {max}자 이하여야 합니다")));
    }
    if value.chars().any(char::is_control) {
        return Err(bad(format!("{what}에 제어 문자를 넣을 수 없습니다")));
    }
    Ok(())
}

/// 앞뒤 공백을 지우고 빈 값은 `None`으로 바꿉니다.
fn optional(what: &str, value: Option<String>, max: usize) -> Result<Option<String>> {
    match value.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(v) => {
            check_text(what, v, max)?;
            Ok(Some(v.to_owned()))
        }
    }
}

fn required(what: &str, value: &str, max: usize) -> Result<String> {
    let v = value.trim();
    if v.is_empty() {
        return Err(bad(format!("{what}을(를) 입력하세요")));
    }
    check_text(what, v, max)?;
    Ok(v.to_owned())
}

/// 볼륨 레이블(WinFsp 한도 32자)
pub fn check_label(label: &str) -> Result<String> {
    required("레이블", label, 32)
}

/// `X:` 드라이브 문자(대문자로 바꿈) 또는 로컬 절대 경로
pub fn normalize_mount_point(mp: &str) -> Result<String> {
    let mp = mp.trim();
    let b = mp.as_bytes();
    if b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return Ok(mp.to_ascii_uppercase());
    }
    check_text("마운트 지점", mp, 240)?;
    if mp.starts_with(r"\\") || mp.starts_with("//") || !Path::new(mp).is_absolute() {
        return Err(bad(
            "마운트 지점은 X: 드라이브 문자 또는 로컬 절대 경로여야 합니다",
        ));
    }
    Ok(mp.to_owned())
}

/// `scheme://host[...]` 형식과 허용 scheme을 확인합니다.
pub fn check_url(what: &str, url: &str, schemes: &[&str]) -> Result<String> {
    let url = required(what, url, 2048)?;
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| bad(format!("{what}는 scheme://host 형식이어야 합니다")))?;
    if !schemes.contains(&scheme.to_ascii_lowercase().as_str()) {
        return Err(bad(format!(
            "{what}는 {}만 쓸 수 있습니다",
            schemes.join(", ")
        )));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || url.contains(char::is_whitespace) {
        return Err(bad(format!("{what}의 호스트가 올바르지 않습니다")));
    }
    if authority.contains('@') {
        return Err(bad(format!(
            "{what}에 자격 증명을 넣지 말고 사용자와 암호 칸을 쓰세요"
        )));
    }
    Ok(url)
}

/// 호스트 이름 또는 IP 주소
pub fn check_host(host: &str) -> Result<String> {
    let host = required("호스트", host, 253)?;
    let ok = host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'));
    if !ok {
        return Err(bad("호스트에는 영문, 숫자, ., -, :만 쓸 수 있습니다"));
    }
    Ok(host)
}

fn check_port(port: u16) -> Result<u16> {
    if port == 0 {
        return Err(bad("포트는 1~65535여야 합니다"));
    }
    Ok(port)
}

fn absolute_path(what: &str, value: &str, want_dir: bool) -> Result<PathBuf> {
    let v = required(what, value, 240)?;
    let p = PathBuf::from(&v);
    if !p.is_absolute() {
        return Err(bad(format!("{what}는 절대 경로여야 합니다")));
    }
    let exists = if want_dir { p.is_dir() } else { p.is_file() };
    if !exists {
        let kind = if want_dir { "폴더" } else { "파일" };
        return Err(bad(format!("{what} {kind}가 없습니다: {v}")));
    }
    Ok(p)
}

fn check_secret(what: &str, s: &Secret) -> Result<()> {
    let v = s.0.expose_secret();
    if v.len() > MAX_SECRET_BYTES || v.contains('\0') {
        return Err(bad(format!(
            "{what}은(는) {MAX_SECRET_BYTES}바이트 이하의 문자열이어야 합니다"
        )));
    }
    Ok(())
}

/// 입력 비밀 값을 자격 증명 참조로 바꾸고 저장 목록에 올립니다. 비어 있으면 참조를 만들지 않습니다.
fn secret_ref(
    id: &str,
    field: &str,
    value: Option<Secret>,
    pending: &mut Vec<PendingSecret>,
) -> Result<Option<SecretSource>> {
    let Some(s) = value.filter(Secret::provided) else {
        return Ok(None);
    };
    check_secret(field, &s)?;
    let target = target_for(id, field);
    pending.push(PendingSecret {
        target: target.clone(),
        value: s.0,
    });
    Ok(Some(SecretSource::Credential { target }))
}

/// WebDAV 인증서 지문을 구분자 없는 대문자 16진수 64자리로 맞춥니다.
fn normalize_pin(pin: Option<String>) -> Result<Option<String>> {
    let Some(pin) = optional("인증서 지문", pin, 200)? else {
        return Ok(None);
    };
    let hex: String = pin
        .chars()
        .filter(|c| !matches!(c, ':' | ' ' | '-'))
        .collect();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(bad("인증서 지문은 64자리 16진수 SHA-256이어야 합니다"));
    }
    Ok(Some(hex.to_ascii_uppercase()))
}

pub fn build_backend(
    id: &str,
    input: BackendInput,
    pending: &mut Vec<PendingSecret>,
) -> Result<BackendConfig> {
    Ok(match input {
        BackendInput::Local { root } => BackendConfig::Local {
            root: absolute_path("로컬 폴더", &root, true)?,
        },
        BackendInput::Webdav {
            url,
            username,
            password,
            pinned_cert_sha256,
        } => BackendConfig::Webdav(WebDavConfig {
            url: check_url("WebDAV URL", &url, &["https", "http"])?,
            username: optional("사용자", username, 256)?,
            password: secret_ref(id, "password", password, pending)?,
            pinned_cert_sha256: normalize_pin(pinned_cert_sha256)?,
        }),
        BackendInput::Sftp {
            host,
            port,
            username,
            password,
            private_key,
            key_passphrase,
            root,
            known_hosts,
        } => {
            let private_key = match optional("개인키", private_key, 240)? {
                Some(p) => Some(absolute_path("개인키", &p, false)?),
                None => None,
            };
            let key_passphrase = if private_key.is_some() {
                secret_ref(id, "key_passphrase", key_passphrase, pending)?
            } else {
                None
            };
            let password = secret_ref(id, "password", password, pending)?;
            if password.is_none() && private_key.is_none() {
                return Err(bad("SFTP는 암호나 개인키 중 하나가 필요합니다"));
            }
            BackendConfig::Sftp(SftpConfig {
                host: check_host(&host)?,
                port: check_port(port)?,
                username: required("사용자", &username, 256)?,
                password,
                private_key,
                key_passphrase,
                root: remote_root(&root)?,
                known_hosts: match optional("known_hosts", known_hosts, 240)? {
                    Some(p) => Some(absolute_path("known_hosts", &p, false)?),
                    None => None,
                },
            })
        }
        BackendInput::S3 {
            endpoint,
            region,
            bucket,
            prefix,
            access_key_id,
            secret_access_key,
            path_style,
        } => {
            let bucket = required("버킷", &bucket, 63)?;
            if bucket.len() < 3
                || !bucket
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-'))
            {
                return Err(bad("버킷 이름은 소문자, 숫자, ., -로 3~63자여야 합니다"));
            }
            let region = required("리전", &region, 64)?;
            if !region
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(bad("리전에는 영문, 숫자, -, _만 쓸 수 있습니다"));
            }
            let access_key_id = required("액세스 키 ID", &access_key_id, 128)?;
            if access_key_id.contains(char::is_whitespace) {
                return Err(bad("액세스 키 ID에 공백을 넣을 수 없습니다"));
            }
            let prefix = prefix.trim().trim_start_matches('/').to_owned();
            check_text("접두사", &prefix, 512)?;
            BackendConfig::S3(S3Config {
                endpoint: check_url("엔드포인트", &endpoint, &["https", "http"])?,
                region,
                bucket,
                prefix,
                access_key_id,
                secret_access_key: secret_ref(
                    id,
                    "secret_access_key",
                    Some(secret_access_key),
                    pending,
                )?
                .ok_or_else(|| bad("비밀 액세스 키를 입력하세요"))?,
                path_style,
            })
        }
        BackendInput::Ftp {
            host,
            port,
            username,
            password,
            tls,
            encoding,
            root,
        } => {
            let encoding = required("문자셋", &encoding, 32)?;
            if !encoding
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(bad("문자셋 이름이 올바르지 않습니다"));
            }
            BackendConfig::Ftp(FtpConfig {
                host: check_host(&host)?,
                port: check_port(port)?,
                username: required("사용자", &username, 256)?,
                password: secret_ref(id, "password", password, pending)?,
                tls,
                encoding,
                root: remote_root(&root)?,
            })
        }
    })
}

/// 원격 루트 경로(`/`로 시작)
fn remote_root(root: &str) -> Result<String> {
    let root = root.trim();
    if root.is_empty() {
        return Ok("/".to_owned());
    }
    check_text("원격 루트", root, 1024)?;
    if !root.starts_with('/') {
        return Err(bad("원격 루트는 /로 시작해야 합니다"));
    }
    Ok(root.to_owned())
}

/// 새 프록시 설정. `existing`이 있으면 암호를 비웠을 때 기존 참조를 유지합니다.
pub fn build_proxy(
    id: &str,
    input: Option<ProxyInput>,
    existing: Option<&ProxyConfig>,
    pending: &mut Vec<PendingSecret>,
) -> Result<Option<ProxyConfig>> {
    let Some(p) = input else {
        return Ok(None);
    };
    if p.url.trim().is_empty() {
        return Ok(None);
    }
    let username = optional("프록시 사용자", p.username, 256)?;
    let password = match &username {
        None => None,
        Some(_) => match secret_ref(id, "proxy_password", p.password, pending)? {
            Some(r) => Some(r),
            None => existing.and_then(|e| e.password.clone()),
        },
    };
    let proxy = ProxyConfig {
        url: p.url.trim().to_owned(),
        username,
        password,
    };
    proxy.parse()?;
    Ok(Some(proxy))
}

pub fn build_drive(input: NewDrive) -> Result<(DriveConfig, Vec<PendingSecret>)> {
    check_id(&input.id)?;
    let mut pending = Vec::new();
    let backend = build_backend(&input.id, input.backend, &mut pending)?;
    let proxy = build_proxy(&input.id, input.proxy, None, &mut pending)?;
    if proxy.is_some() && matches!(backend, BackendConfig::Local { .. }) {
        return Err(bad("로컬 폴더 드라이브에는 프록시를 쓸 수 없습니다"));
    }
    let drive = DriveConfig {
        label: check_label(&input.label)?,
        mount_point: normalize_mount_point(&input.mount_point)?,
        id: input.id,
        read_only: input.read_only,
        cache: CacheConfig::default(),
        transfer: TransferConfig::default(),
        auto_mount: input.auto_mount,
        proxy,
        backend,
    };
    Ok((drive, pending))
}

/// 비밀 값을 먼저 저장하고 설정을 저장합니다. 어느 단계든 실패하면 이번에 저장한 비밀 값을 지웁니다.
fn commit(
    cfg: &Config,
    cfg_path: &Path,
    store: &dyn SecretStore,
    pending: &[PendingSecret],
) -> Result<()> {
    let mut stored = Vec::new();
    let r = (|| {
        for p in pending {
            store.set(&p.target, &p.value)?;
            stored.push(p.target.as_str());
        }
        cfg.save(cfg_path)
    })();
    if r.is_err() {
        for t in stored {
            let _ = store.delete(t);
        }
    }
    r
}

pub fn add_drive(cfg_path: &Path, store: &dyn SecretStore, input: NewDrive) -> Result<String> {
    let (drive, pending) = build_drive(input)?;
    let mut cfg = Config::load(cfg_path)?;
    if cfg.drive(&drive.id).is_some() {
        return Err(bad(format!("드라이브 {}가 이미 있습니다", drive.id)));
    }
    let id = drive.id.clone();
    cfg.drives.push(drive);
    cfg.validate()?;
    commit(&cfg, cfg_path, store, &pending)?;
    Ok(id)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CacheInput {
    pub metadata_ttl_secs: u64,
    pub block_size_mib: u32,
    pub memory_limit_mib: u32,
    pub poll_interval_secs: u64,
    pub disk_limit_mib: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransferInput {
    pub upload_kib_per_sec: u64,
    pub download_kib_per_sec: u64,
}

/// 속성 화면에서 바꿀 수 있는 값. 연결 정보는 바꾸지 않습니다.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DrivePatch {
    pub label: String,
    pub mount_point: String,
    pub read_only: bool,
    pub auto_mount: bool,
    pub cache: CacheInput,
    pub transfer: TransferInput,
    pub proxy: Option<ProxyInput>,
}

fn in_range<T: PartialOrd + std::fmt::Display>(what: &str, v: T, min: T, max: T) -> Result<T> {
    if v < min || v > max {
        return Err(bad(format!("{what}은(는) {min}~{max} 범위여야 합니다")));
    }
    Ok(v)
}

fn build_cache(c: CacheInput) -> Result<CacheConfig> {
    Ok(CacheConfig {
        metadata_ttl_secs: in_range("메타 캐시 유지 시간", c.metadata_ttl_secs, 0, 86_400)?,
        block_size_mib: in_range("블록 크기", c.block_size_mib, 1, 64)?,
        memory_limit_mib: in_range("캐시 메모리 상한", c.memory_limit_mib, 8, 8192)?,
        poll_interval_secs: in_range("변경 확인 주기", c.poll_interval_secs, 0, 86_400)?,
        disk_limit_mib: in_range("디스크 캐시 상한", c.disk_limit_mib, 0, 1 << 20)?,
    })
}

fn build_transfer(t: TransferInput) -> Result<TransferConfig> {
    const MAX: u64 = 10_000_000;
    Ok(TransferConfig {
        upload_kib_per_sec: in_range("업로드 제한", t.upload_kib_per_sec, 0, MAX)?,
        download_kib_per_sec: in_range("다운로드 제한", t.download_kib_per_sec, 0, MAX)?,
    })
}

pub fn update_drive(
    cfg_path: &Path,
    store: &dyn SecretStore,
    id: &str,
    patch: DrivePatch,
) -> Result<()> {
    let mut cfg = Config::load(cfg_path)?;
    let d = cfg
        .drives
        .iter_mut()
        .find(|d| d.id == id)
        .ok_or_else(|| bad(format!("드라이브 {id}가 없습니다")))?;
    let mut pending = Vec::new();
    let proxy = build_proxy(id, patch.proxy, d.proxy.as_ref(), &mut pending)?;
    if proxy.is_some() && matches!(d.backend, BackendConfig::Local { .. }) {
        return Err(bad("로컬 폴더 드라이브에는 프록시를 쓸 수 없습니다"));
    }
    d.label = check_label(&patch.label)?;
    d.mount_point = normalize_mount_point(&patch.mount_point)?;
    d.read_only = patch.read_only;
    d.auto_mount = patch.auto_mount;
    d.cache = build_cache(patch.cache)?;
    d.transfer = build_transfer(patch.transfer)?;
    let dropped = stale_proxy_target(id, d.proxy.as_ref(), proxy.as_ref());
    d.proxy = proxy;
    cfg.validate()?;
    commit(&cfg, cfg_path, store, &pending)?;
    // 프록시 암호를 더 쓰지 않으면 저장한 값도 지웁니다.
    if let Some(t) = dropped {
        let _ = store.delete(&t);
    }
    Ok(())
}

/// 이전 프록시 설정에만 있던 이 드라이브의 자격 증명 대상
fn stale_proxy_target(
    id: &str,
    old: Option<&ProxyConfig>,
    new: Option<&ProxyConfig>,
) -> Option<String> {
    let target = target_for(id, "proxy_password");
    let refers = |p: Option<&ProxyConfig>| {
        matches!(p.and_then(|p| p.password.as_ref()),
            Some(SecretSource::Credential { target: t }) if *t == target)
    };
    (refers(old) && !refers(new)).then_some(target)
}

/// 드라이브 설정에서 비밀 값 참조 칸을 찾습니다.
fn secret_slot<'a>(d: &'a mut DriveConfig, field: &str) -> Result<&'a mut Option<SecretSource>> {
    let missing = || bad(format!("이 드라이브에는 {field} 항목이 없습니다"));
    match (field, &mut d.backend) {
        ("password", BackendConfig::Webdav(c)) => Ok(&mut c.password),
        ("password", BackendConfig::Sftp(c)) => Ok(&mut c.password),
        ("key_passphrase", BackendConfig::Sftp(c)) if c.private_key.is_some() => {
            Ok(&mut c.key_passphrase)
        }
        ("password", BackendConfig::Ftp(c)) => Ok(&mut c.password),
        ("proxy_password", _) => match &mut d.proxy {
            Some(p) if p.username.is_some() => Ok(&mut p.password),
            _ => Err(missing()),
        },
        _ => Err(missing()),
    }
}

/// 저장한 비밀 값을 바꿉니다. 설정이 다른 참조(환경 변수 등)를 쓰고 있으면 자격 증명 참조로 바꿉니다.
pub fn set_drive_secret(
    cfg_path: &Path,
    store: &dyn SecretStore,
    id: &str,
    field: &str,
    value: Secret,
) -> Result<()> {
    if !value.provided() {
        return Err(bad("새 값을 입력하세요"));
    }
    check_secret(field, &value)?;
    let mut cfg = Config::load(cfg_path)?;
    let d = cfg
        .drives
        .iter_mut()
        .find(|d| d.id == id)
        .ok_or_else(|| bad(format!("드라이브 {id}가 없습니다")))?;
    let target = target_for(id, field);
    let source = SecretSource::Credential {
        target: target.clone(),
    };
    if field == "secret_access_key" {
        let BackendConfig::S3(c) = &mut d.backend else {
            return Err(bad(format!("이 드라이브에는 {field} 항목이 없습니다")));
        };
        if c.secret_access_key == source {
            return store.set(&target, &value.0);
        }
        c.secret_access_key = source;
    } else {
        let slot = secret_slot(d, field)?;
        if slot.as_ref() == Some(&source) {
            return store.set(&target, &value.0);
        }
        *slot = Some(source);
    }
    commit(
        &cfg,
        cfg_path,
        store,
        &[PendingSecret {
            target,
            value: value.0,
        }],
    )
}

/// 설정에서 드라이브를 지우고 이 드라이브용 자격 증명도 지웁니다. 지운 자격 증명 수를 돌려줍니다.
pub fn delete_drive(cfg_path: &Path, store: &dyn SecretStore, id: &str) -> Result<usize> {
    let mut cfg = Config::load(cfg_path)?;
    let before = cfg.drives.len();
    cfg.drives.retain(|d| d.id != id);
    if cfg.drives.len() == before {
        return Err(bad(format!("드라이브 {id}가 없습니다")));
    }
    cfg.save(cfg_path)?;
    let targets = store.list(&format!("ExpanDrv/{id}/"))?;
    for t in &targets {
        store.delete(t)?;
    }
    Ok(targets.len())
}

// ---- 화면에 보낼 읽기 전용 정보(비밀 값은 넣지 않음) ----

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Field {
    pub label: &'static str,
    pub value: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretInfo {
    pub field: &'static str,
    pub label: &'static str,
    /// `credential`, `env`, `none`
    pub source: &'static str,
    /// 자격 증명 대상 이름 또는 환경 변수 이름
    pub reference: Option<String>,
    /// 자격 증명 관리자에 값이 있는지
    pub stored: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyView {
    pub url: String,
    pub username: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveDetail {
    pub id: String,
    pub label: String,
    pub mount_point: String,
    pub read_only: bool,
    pub auto_mount: bool,
    pub backend: &'static str,
    pub connection: Vec<Field>,
    pub secrets: Vec<SecretInfo>,
    pub cache: CacheInput,
    pub transfer: TransferInput,
    pub proxy: Option<ProxyView>,
}

fn secret_info(
    field: &'static str,
    label: &'static str,
    src: Option<&SecretSource>,
    stored: &[String],
) -> SecretInfo {
    let (source, reference, has) = match src {
        Some(SecretSource::Credential { target }) => {
            ("credential", Some(target.clone()), stored.contains(target))
        }
        Some(SecretSource::Env { var }) => ("env", Some(var.clone()), false),
        None => ("none", None, false),
    };
    SecretInfo {
        field,
        label,
        source,
        reference,
        stored: has,
    }
}

pub fn drive_detail(d: &DriveConfig, stored: &[String]) -> DriveDetail {
    let f = |label, value: &str| Field {
        label,
        value: value.to_owned(),
    };
    let path = |p: &Path| p.display().to_string();
    let mut secrets = Vec::new();
    let connection = match &d.backend {
        BackendConfig::Local { root } => vec![f("로컬 폴더", &path(root))],
        BackendConfig::Webdav(c) => {
            secrets.push(secret_info("password", "암호", c.password.as_ref(), stored));
            vec![
                f("URL", &c.url),
                f("사용자", c.username.as_deref().unwrap_or("-")),
                f(
                    "인증서 지문",
                    c.pinned_cert_sha256.as_deref().unwrap_or("-"),
                ),
            ]
        }
        BackendConfig::Sftp(c) => {
            secrets.push(secret_info("password", "암호", c.password.as_ref(), stored));
            if c.private_key.is_some() {
                secrets.push(secret_info(
                    "key_passphrase",
                    "개인키 암호문",
                    c.key_passphrase.as_ref(),
                    stored,
                ));
            }
            vec![
                f("호스트", &format!("{}:{}", c.host, c.port)),
                f("사용자", &c.username),
                f(
                    "개인키",
                    &c.private_key.as_deref().map(path).unwrap_or("-".into()),
                ),
                f("원격 루트", &c.root),
                f(
                    "known_hosts",
                    &c.known_hosts
                        .as_deref()
                        .map(path)
                        .unwrap_or("기본 위치".into()),
                ),
            ]
        }
        BackendConfig::S3(c) => {
            secrets.push(secret_info(
                "secret_access_key",
                "비밀 액세스 키",
                Some(&c.secret_access_key),
                stored,
            ));
            vec![
                f("엔드포인트", &c.endpoint),
                f("리전", &c.region),
                f("버킷", &c.bucket),
                f("접두사", if c.prefix.is_empty() { "-" } else { &c.prefix }),
                f("액세스 키 ID", &c.access_key_id),
                f(
                    "주소 방식",
                    if c.path_style {
                        "경로 방식"
                    } else {
                        "가상 호스트"
                    },
                ),
            ]
        }
        BackendConfig::Ftp(c) => {
            secrets.push(secret_info("password", "암호", c.password.as_ref(), stored));
            vec![
                f("호스트", &format!("{}:{}", c.host, c.port)),
                f("사용자", &c.username),
                f(
                    "TLS",
                    match c.tls {
                        FtpTls::None => "사용 안 함",
                        FtpTls::Explicit => "명시적(AUTH TLS)",
                        FtpTls::Implicit => "암묵적(990)",
                    },
                ),
                f("문자셋", &c.encoding),
                f("원격 루트", &c.root),
            ]
        }
    };
    if let Some(p) = &d.proxy
        && p.username.is_some()
    {
        secrets.push(secret_info(
            "proxy_password",
            "프록시 암호",
            p.password.as_ref(),
            stored,
        ));
    }
    DriveDetail {
        id: d.id.clone(),
        label: d.label.clone(),
        mount_point: d.mount_point.clone(),
        read_only: d.read_only,
        auto_mount: d.auto_mount,
        backend: d.backend.kind(),
        connection,
        secrets,
        cache: CacheInput {
            metadata_ttl_secs: d.cache.metadata_ttl_secs,
            block_size_mib: d.cache.block_size_mib,
            memory_limit_mib: d.cache.memory_limit_mib,
            poll_interval_secs: d.cache.poll_interval_secs,
            disk_limit_mib: d.cache.disk_limit_mib,
        },
        transfer: TransferInput {
            upload_kib_per_sec: d.transfer.upload_kib_per_sec,
            download_kib_per_sec: d.transfer.download_kib_per_sec,
        },
        proxy: d.proxy.as_ref().map(|p| ProxyView {
            url: p.url.clone(),
            username: p.username.clone(),
        }),
    }
}

#[cfg(test)]
#[path = "drives_tests.rs"]
mod tests;
