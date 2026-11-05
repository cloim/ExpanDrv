use super::*;
use std::cell::RefCell;
use std::collections::BTreeMap;

const PW: &str = "pw-Very$ecret-7";

/// 메모리 저장소. `fail_on` 대상을 저장하려 하면 실패합니다.
#[derive(Default)]
struct MemStore {
    items: RefCell<BTreeMap<String, String>>,
    fail_on: Option<&'static str>,
}

impl SecretStore for MemStore {
    fn set(&self, target: &str, value: &SecretString) -> Result<()> {
        if self.fail_on == Some(target) {
            return Err(Error::Config("저장 실패(테스트)".into()));
        }
        self.items
            .borrow_mut()
            .insert(target.into(), value.expose_secret().into());
        Ok(())
    }
    fn delete(&self, target: &str) -> Result<()> {
        self.items
            .borrow_mut()
            .remove(target)
            .map(drop)
            .ok_or(Error::NotFound)
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        Ok(self
            .items
            .borrow()
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect())
    }
}

fn json<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> T {
    serde_json::from_value(v).unwrap()
}

fn webdav_input(id: &str, mp: &str) -> NewDrive {
    json(serde_json::json!({
        "id": id,
        "label": "NAS",
        "mountPoint": mp,
        "readOnly": false,
        "autoMount": true,
        "proxy": { "url": "socks5h://proxy.example.com:1080", "username": "pu", "password": PW },
        "backend": {
            "type": "webdav",
            "url": "https://dav.example.com/remote.php/dav/files/me/",
            "username": "me",
            "password": PW,
            "pinnedCertSha256": ""
        }
    }))
}

#[test]
fn validates_ids_labels_and_mount_points() {
    for ok in ["nas", "a-b_1", "X"] {
        check_id(ok).unwrap();
    }
    for bad in ["", "../x", "a b", "드라이브", &"a".repeat(33)] {
        assert!(check_id(bad).is_err(), "{bad}");
    }
    assert_eq!(check_label("  NAS ").unwrap(), "NAS");
    assert!(check_label(" ").is_err());
    assert!(check_label("a\u{7}b").is_err());
    assert!(check_label(&"가".repeat(33)).is_err());

    assert_eq!(normalize_mount_point("w:").unwrap(), "W:");
    assert_eq!(
        normalize_mount_point(r"C:\Mounts\nas").unwrap(),
        r"C:\Mounts\nas"
    );
    for bad in [
        "W",
        "1:",
        "relative",
        r"\\server\share",
        "//server/share",
        "",
    ] {
        assert!(normalize_mount_point(bad).is_err(), "{bad}");
    }
}

#[test]
fn validates_urls_and_hosts() {
    let web = ["https", "http"];
    check_url("URL", "https://dav.example.com/", &web).unwrap();
    check_url("URL", "HTTP://dav.example.com:8080/x", &web).unwrap();
    for bad in [
        "ftp://dav.example.com/",
        "file:///C:/x",
        "javascript:alert(1)",
        "https://",
        "https://user:pw@dav.example.com/",
        "https://dav.example.com/a b",
        "dav.example.com",
    ] {
        assert!(check_url("URL", bad, &web).is_err(), "{bad}");
    }
    check_host("sftp.example.com").unwrap();
    check_host("[::1]").unwrap();
    for bad in ["", "a b", "host/x", "u@host"] {
        assert!(check_host(bad).is_err(), "{bad}");
    }
}

#[test]
fn rejects_unknown_fields_and_bad_backend_values() {
    // 모르는 칸은 받지 않습니다(설정에 엉뚱한 값이 섞이지 않게).
    let r: std::result::Result<NewDrive, _> = serde_json::from_value(serde_json::json!({
        "id": "x", "label": "x", "mountPoint": "X:", "readOnly": false, "autoMount": false,
        "proxy": null, "backend": { "type": "local", "root": "C:\\", "extra": 1 }
    }));
    assert!(r.is_err());

    let mut p = Vec::new();
    let s3 = |bucket: &str, secret: &str| -> BackendInput {
        json(serde_json::json!({
            "type": "s3", "endpoint": "https://s3.example.com", "region": "us-east-1",
            "bucket": bucket, "prefix": "/photos/", "accessKeyId": "AKIAEXAMPLE",
            "secretAccessKey": secret, "pathStyle": true
        }))
    };
    assert!(build_backend("b", s3("Bad_Bucket", PW), &mut p).is_err());
    assert!(build_backend("b", s3("bkt", ""), &mut p).is_err());
    let BackendConfig::S3(c) = build_backend("b", s3("bkt", PW), &mut p).unwrap() else {
        panic!()
    };
    assert_eq!(c.prefix, "photos/");

    let sftp: BackendInput = json(serde_json::json!({
        "type": "sftp", "host": "sftp.example.com", "port": 22, "username": "me",
        "password": "", "privateKey": "", "keyPassphrase": "", "root": "/", "knownHosts": ""
    }));
    // 암호와 개인키가 모두 없으면 거부합니다.
    assert!(build_backend("s", sftp, &mut p).is_err());

    let dav = |pin: &str| -> BackendInput {
        json(serde_json::json!({
            "type": "webdav", "url": "https://dav.example.com/", "username": null,
            "password": null, "pinnedCertSha256": pin
        }))
    };
    assert!(build_backend("w", dav("zz"), &mut p).is_err());
    let BackendConfig::Webdav(w) = build_backend("w", dav(&"ab:".repeat(32)), &mut p).unwrap()
    else {
        panic!()
    };
    assert_eq!(w.pinned_cert_sha256.unwrap(), "AB".repeat(32));
}

#[test]
fn add_stores_secrets_as_references_only() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let store = MemStore::default();
    add_drive(&cfg_path, &store, webdav_input("nas", "w:")).unwrap();

    let text = std::fs::read_to_string(&cfg_path).unwrap();
    assert!(
        !text.contains(PW),
        "설정 파일에 비밀 값이 들어가면 안 됩니다"
    );
    let cfg = Config::load(&cfg_path).unwrap();
    let d = cfg.drive("nas").unwrap();
    assert_eq!(d.mount_point, "W:");
    assert!(d.auto_mount);
    let BackendConfig::Webdav(w) = &d.backend else {
        panic!()
    };
    assert_eq!(
        w.password,
        Some(SecretSource::Credential {
            target: "ExpanDrv/nas/password".into()
        })
    );
    assert_eq!(
        store.list("ExpanDrv/nas/").unwrap(),
        ["ExpanDrv/nas/password", "ExpanDrv/nas/proxy_password"]
    );
    assert_eq!(store.items.borrow()["ExpanDrv/nas/password"], PW);

    // 화면으로 돌려보내는 정보에도 값은 없습니다.
    let detail = drive_detail(d, &store.list("ExpanDrv/nas/").unwrap());
    let out = serde_json::to_string(&detail).unwrap();
    assert!(!out.contains(PW));
    assert!(detail.secrets.iter().all(|s| s.stored));
    assert_eq!(
        format!("{:?}", Secret(SecretString::from(PW))),
        "Secret(***)"
    );

    // 같은 id나 같은 마운트 지점은 거부하고 아무것도 저장하지 않습니다.
    let store2 = MemStore::default();
    assert!(add_drive(&cfg_path, &store2, webdav_input("nas", "V:")).is_err());
    assert!(add_drive(&cfg_path, &store2, webdav_input("nas2", "W:")).is_err());
    assert!(store2.items.borrow().is_empty());
}

#[test]
fn add_rolls_back_secrets_when_saving_fails() {
    let dir = tempfile::tempdir().unwrap();
    // 상위 경로가 파일이라 설정을 저장할 수 없습니다.
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, "").unwrap();
    let store = MemStore::default();
    assert!(
        add_drive(
            &blocker.join("config.toml"),
            &store,
            webdav_input("nas", "W:")
        )
        .is_err()
    );
    assert!(store.items.borrow().is_empty());

    // 두 번째 비밀 값 저장이 실패하면 첫 번째도 지우고 설정은 그대로입니다.
    let cfg_path = dir.path().join("config.toml");
    let store = MemStore {
        fail_on: Some("ExpanDrv/nas/proxy_password"),
        ..Default::default()
    };
    assert!(add_drive(&cfg_path, &store, webdav_input("nas", "W:")).is_err());
    assert!(store.items.borrow().is_empty());
    assert!(!cfg_path.exists());
}

fn patch(proxy: serde_json::Value) -> DrivePatch {
    json(serde_json::json!({
        "label": "NAS 2", "mountPoint": "v:", "readOnly": true, "autoMount": false,
        "cache": { "metadataTtlSecs": 10, "blockSizeMib": 8, "memoryLimitMib": 128, "pollIntervalSecs": 0, "diskLimitMib": 256 },
        "transfer": { "uploadKibPerSec": 512, "downloadKibPerSec": 0 },
        "proxy": proxy
    }))
}

#[test]
fn update_keeps_or_drops_proxy_password() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let store = MemStore::default();
    add_drive(&cfg_path, &store, webdav_input("nas", "W:")).unwrap();

    // 암호를 비우면 기존 참조를 유지합니다.
    let keep = serde_json::json!({ "url": "http://proxy.example.com:3128", "username": "pu", "password": "" });
    update_drive(&cfg_path, &store, "nas", patch(keep)).unwrap();
    let cfg = Config::load(&cfg_path).unwrap();
    let d = cfg.drive("nas").unwrap();
    assert_eq!((d.label.as_str(), d.mount_point.as_str()), ("NAS 2", "V:"));
    assert!(d.read_only && !d.auto_mount);
    assert_eq!((d.cache.block_size_mib, d.cache.disk_limit_mib), (8, 256));
    assert_eq!(d.transfer.upload_kib_per_sec, 512);
    assert!(d.proxy.as_ref().unwrap().password.is_some());
    assert!(
        store
            .items
            .borrow()
            .contains_key("ExpanDrv/nas/proxy_password")
    );

    // 프록시를 지우면 저장한 프록시 암호도 지웁니다.
    update_drive(&cfg_path, &store, "nas", patch(serde_json::Value::Null)).unwrap();
    assert!(Config::load(&cfg_path).unwrap().drives[0].proxy.is_none());
    assert!(
        !store
            .items
            .borrow()
            .contains_key("ExpanDrv/nas/proxy_password")
    );

    // 범위를 벗어난 값은 거부합니다.
    let mut bad = patch(serde_json::Value::Null);
    bad.cache.block_size_mib = 0;
    assert!(update_drive(&cfg_path, &store, "nas", bad).is_err());
    assert!(update_drive(&cfg_path, &store, "none", patch(serde_json::Value::Null)).is_err());
}

#[test]
fn set_secret_switches_env_reference_to_credential() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        r#"version = 1
[[drive]]
id = "ftp1"
label = "FTP"
mount_point = "F:"
[drive.backend]
type = "ftp"
host = "ftp.example.com"
username = "me"
password = { from = "env", var = "FTP_PASSWORD" }
"#,
    )
    .unwrap();
    let store = MemStore::default();
    let secret = || Secret(SecretString::from(PW));
    assert!(set_drive_secret(&cfg_path, &store, "ftp1", "key_passphrase", secret()).is_err());
    assert!(
        set_drive_secret(
            &cfg_path,
            &store,
            "ftp1",
            "password",
            Secret(SecretString::from(""))
        )
        .is_err()
    );
    set_drive_secret(&cfg_path, &store, "ftp1", "password", secret()).unwrap();
    let cfg = Config::load(&cfg_path).unwrap();
    let BackendConfig::Ftp(f) = &cfg.drives[0].backend else {
        panic!()
    };
    assert_eq!(
        f.password,
        Some(SecretSource::Credential {
            target: "ExpanDrv/ftp1/password".into()
        })
    );
    assert!(!std::fs::read_to_string(&cfg_path).unwrap().contains(PW));
    assert_eq!(store.items.borrow()["ExpanDrv/ftp1/password"], PW);
}

#[test]
fn delete_removes_drive_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let store = MemStore::default();
    add_drive(&cfg_path, &store, webdav_input("nas", "W:")).unwrap();
    store
        .set("ExpanDrv/other/password", &SecretString::from("x"))
        .unwrap();
    assert_eq!(delete_drive(&cfg_path, &store, "nas").unwrap(), 2);
    assert!(Config::load(&cfg_path).unwrap().drives.is_empty());
    assert_eq!(
        store.list("ExpanDrv/").unwrap(),
        ["ExpanDrv/other/password"]
    );
    assert!(delete_drive(&cfg_path, &store, "nas").is_err());
}

#[test]
fn typed_secrets_resolve_only_entered_values() {
    let mut pending = Vec::new();
    let backend = build_backend(
        "connection-test",
        json(serde_json::json!({
            "type": "ftp", "host": "ftp.example.com", "port": 21, "username": "me",
            "password": PW, "tls": "explicit", "encoding": "utf-8", "root": ""
        })),
        &mut pending,
    )
    .unwrap();
    let BackendConfig::Ftp(f) = backend else {
        panic!()
    };
    assert_eq!(f.root, "/");
    let typed = TypedSecrets::new(pending);
    let got = typed.resolve(f.password.as_ref().unwrap()).unwrap();
    assert_eq!(got.expose_secret(), PW);
    assert!(
        typed
            .resolve(&SecretSource::Env { var: "PATH".into() })
            .is_err()
    );
}
