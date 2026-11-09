//! 기존 자격 증명 참조를 유지하며 공개 연결 필드만 고칩니다.
use super::*;
use std::collections::BTreeMap;

pub fn connection_fields(backend: &BackendConfig) -> BTreeMap<String, String> {
    let fields: Vec<(&str, String)> = match backend {
        BackendConfig::Local { root } => vec![("root", root.display().to_string())],
        BackendConfig::Webdav(c) => vec![
            ("url", c.url.clone()),
            ("username", c.username.clone().unwrap_or_default()),
            (
                "pinnedCertSha256",
                c.pinned_cert_sha256.clone().unwrap_or_default(),
            ),
        ],
        BackendConfig::Sftp(c) => vec![
            ("host", c.host.clone()),
            ("port", c.port.to_string()),
            ("username", c.username.clone()),
            ("root", c.root.clone()),
            (
                "privateKey",
                c.private_key
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            ),
            (
                "knownHosts",
                c.known_hosts
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            ),
        ],
        BackendConfig::S3(c) => vec![
            ("endpoint", c.endpoint.clone()),
            ("region", c.region.clone()),
            ("bucket", c.bucket.clone()),
            ("prefix", c.prefix.clone()),
            ("accessKeyId", c.access_key_id.clone()),
            ("pathStyle", c.path_style.to_string()),
        ],
        BackendConfig::Ftp(c) => vec![
            (
                "pinnedCertSha256",
                c.pinned_cert_sha256.clone().unwrap_or_default(),
            ),
            ("host", c.host.clone()),
            ("port", c.port.to_string()),
            ("username", c.username.clone()),
            ("root", c.root.clone()),
            ("encoding", c.encoding.clone()),
            (
                "tls",
                match c.tls {
                    FtpTls::None => "none",
                    FtpTls::Explicit => "explicit",
                    FtpTls::Implicit => "implicit",
                }
                .into(),
            ),
        ],
    };
    fields
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

pub fn edited_backend(
    backend: &BackendConfig,
    fields: &BTreeMap<String, String>,
) -> Result<BackendConfig> {
    if connection_fields(backend).keys().ne(fields.keys()) {
        return Err(bad("연결 필드가 현재 저장소 종류와 일치해야 합니다"));
    }
    for value in fields.values() {
        check_text("연결 정보", value, 2048)?;
    }
    let v = |key: &str| fields[key].trim().to_owned();
    let port = || {
        v("port")
            .parse::<u16>()
            .map_err(|_| bad("포트는 1~65535 사이 정수여야 합니다"))
            .and_then(check_port)
    };
    let path = |key: &str| -> Result<Option<std::path::PathBuf>> {
        optional("파일 경로", Some(v(key)), 240)?
            .map(|p| absolute_path("파일 경로", &p, false))
            .transpose()
    };
    let mut out = backend.clone();
    match &mut out {
        BackendConfig::Local { root } => *root = absolute_path("로컬 폴더", &v("root"), true)?,
        BackendConfig::Webdav(c) => {
            c.url = check_url("WebDAV URL", &v("url"), &["https", "http"])?;
            c.username = optional("사용자", Some(v("username")), 256)?;
            c.pinned_cert_sha256 = normalize_pin(Some(v("pinnedCertSha256")))?;
        }
        BackendConfig::Sftp(c) => {
            c.host = check_host(&v("host"))?;
            c.port = port()?;
            c.username = required("사용자", &v("username"), 256)?;
            c.root = remote_root(&v("root"))?;
            c.private_key = path("privateKey")?;
            c.known_hosts = path("knownHosts")?;
            if c.private_key.is_none() {
                c.key_passphrase = None;
                if c.password.is_none() {
                    return Err(bad("SFTP는 암호나 개인키 중 하나가 필요합니다"));
                }
            }
        }
        BackendConfig::S3(c) => {
            c.endpoint = check_url("엔드포인트", &v("endpoint"), &["https", "http"])?;
            c.region = required("리전", &v("region"), 64)?;
            if !c
                .region
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            {
                return Err(bad("리전 이름을 확인하세요"));
            }
            c.bucket = required("버킷", &v("bucket"), 63)?;
            if c.bucket.len() < 3
                || !c
                    .bucket
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-'))
            {
                return Err(bad("버킷 이름을 확인하세요"));
            }
            c.prefix = v("prefix").trim_start_matches('/').into();
            check_text("접두사", &c.prefix, 512)?;
            c.access_key_id = required("액세스 키 ID", &v("accessKeyId"), 128)?;
            if c.access_key_id.contains(char::is_whitespace) {
                return Err(bad("액세스 키 ID의 공백을 확인하세요"));
            }
            c.path_style = v("pathStyle")
                .parse()
                .map_err(|_| bad("주소 방식 값이 올바르지 않습니다"))?;
        }
        BackendConfig::Ftp(c) => {
            c.host = check_host(&v("host"))?;
            c.port = port()?;
            c.username = required("사용자", &v("username"), 256)?;
            c.root = remote_root(&v("root"))?;
            c.encoding = required("문자셋", &v("encoding"), 32)?;
            if !c
                .encoding
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            {
                return Err(bad("문자셋 이름을 확인하세요"));
            }
            c.tls = match v("tls").as_str() {
                "none" => FtpTls::None,
                "explicit" => FtpTls::Explicit,
                "implicit" => FtpTls::Implicit,
                _ => return Err(bad("TLS 방식을 확인하세요")),
            };
            c.pinned_cert_sha256 = normalize_pin(Some(v("pinnedCertSha256")))?;
            if c.tls == FtpTls::None && c.pinned_cert_sha256.is_some() {
                return Err(bad("인증서 지문을 사용하려면 TLS를 선택하세요"));
            }
        }
    }
    Ok(out)
}

/// 연결 대상 변경 시 캐시를 따로 보관합니다. 대기 파일은 원래 대상에서 먼저 처리합니다.
pub fn save_connection(
    cfg_path: &Path,
    expected: &Config,
    id: &str,
    backend: BackendConfig,
    cache_dir: &Path,
) -> Result<()> {
    let mut cfg = Config::load(cfg_path)?;
    if &cfg != expected {
        return Err(bad("설정이 바뀌었습니다. 새로 고침한 뒤 다시 확인하세요"));
    }
    let d = cfg
        .drives
        .iter_mut()
        .find(|d| d.id == id)
        .ok_or_else(|| bad("드라이브가 없습니다"))?;
    if d.backend == backend {
        return Ok(());
    }
    let staging = cache_dir.join("staging");
    match std::fs::read_dir(&staging) {
        Ok(mut entries) => {
            if entries.next().transpose()?.is_some() {
                return Err(bad(
                    "로컬 보관 파일을 기존 연결에서 먼저 업로드하세요. 연결 정보와 파일은 유지됩니다",
                ));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    d.backend = backend;
    cfg.validate()?;
    let archive = cache_dir.with_file_name(format!(
        "{id}.previous-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    let archived = match std::fs::rename(cache_dir, &archive) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    if let Err(e) = cfg.save(cfg_path) {
        if archived {
            std::fs::rename(&archive, cache_dir)?;
        }
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_connection_keeps_credentials_and_rejects_unknown_fields() {
        let old = BackendConfig::Webdav(WebDavConfig {
            url: "https://old.example.com/".into(),
            username: Some("me".into()),
            password: Some(SecretSource::Env {
                var: "TEST_PASSWORD".into(),
            }),
            pinned_cert_sha256: None,
        });
        let mut fields = connection_fields(&old);
        assert!(!fields.contains_key("password"));
        fields.insert("url".into(), "https://new.example.com/".into());
        let new = edited_backend(&old, &fields).unwrap();
        let BackendConfig::Webdav(new) = new else {
            panic!()
        };
        let BackendConfig::Webdav(old_config) = &old else {
            panic!()
        };
        assert_eq!(new.password, old_config.password);
        fields.insert("password".into(), "unexpected".into());
        assert!(edited_backend(&old, &fields).is_err());
        fields.remove("password");
        fields.insert("url".into(), "https://user:password@example.com/".into());
        assert!(edited_backend(&old, &fields).is_err());
    }

    #[test]
    fn pending_files_block_destination_change_and_cache_is_archived_afterwards() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let new_root = dir.path().join("next");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&new_root).unwrap();
        let cache = dir.path().join("cache").join("drive");
        std::fs::create_dir_all(cache.join("staging")).unwrap();
        std::fs::write(cache.join("meta.db"), "old cache").unwrap();
        let pending = cache.join("staging/pending");
        std::fs::write(&pending, "unsent data").unwrap();
        let path = dir.path().join("config.toml");
        let mut cfg = Config::default();
        cfg.drives.push(DriveConfig {
            id: "drive".into(),
            label: "test".into(),
            mount_point: "W:".into(),
            read_only: false,
            auto_mount: false,
            cache: CacheConfig::default(),
            transfer: TransferConfig::default(),
            proxy: None,
            backend: BackendConfig::Local { root },
        });
        cfg.save(&path).unwrap();
        let next = BackendConfig::Local { root: new_root };
        assert!(save_connection(&path, &cfg, "drive", next.clone(), &cache).is_err());
        assert_eq!(std::fs::read_to_string(&pending).unwrap(), "unsent data");
        assert_eq!(Config::load(&path).unwrap(), cfg);
        std::fs::remove_file(pending).unwrap();
        save_connection(&path, &cfg, "drive", next.clone(), &cache).unwrap();
        assert_eq!(Config::load(&path).unwrap().drives[0].backend, next);
        assert!(!cache.exists());
        let archived = std::fs::read_dir(cache.parent().unwrap())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            std::fs::read_to_string(archived.join("meta.db")).unwrap(),
            "old cache"
        );
        assert!(
            save_connection(&path, &cfg, "drive", cfg.drives[0].backend.clone(), &cache).is_err(),
            "검사 중 바뀐 설정 유지"
        );
    }
}
