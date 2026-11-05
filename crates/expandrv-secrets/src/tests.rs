//! 실제 자격 증명 관리자를 씁니다. 테스트마다 고유한 접두사를 쓰고 끝나면 지웁니다.

use super::*;
use std::hash::{BuildHasher, RandomState};

/// 고유한 대상 접두사. 드롭할 때(테스트가 실패해도) 그 아래 항목을 모두 지웁니다.
struct Scope(String);

impl Scope {
    fn new(name: &str) -> Self {
        let id = RandomState::new().hash_one(std::process::id());
        Self(format!("ExpanDrv-test/{name}-{id:016x}/"))
    }

    fn target(&self, field: &str) -> String {
        format!("{}{field}", self.0)
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        for name in CredentialStore.list(&self.0).unwrap_or_default() {
            let _ = CredentialStore.delete(&name);
        }
    }
}

fn secret(s: &str) -> SecretString {
    SecretString::from(s)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn target_name_format() {
    assert_eq!(target_for("nas", "password"), "ExpanDrv/nas/password");
}

#[test]
fn roundtrips_ascii_and_unicode() {
    let scope = Scope::new("roundtrips");
    for (field, value) in [("ascii", "p@ss w0rd!"), ("unicode", "비밀번호-🔐-Grüße")] {
        let t = scope.target(field);
        CredentialStore.set(&t, &secret(value)).unwrap();
        assert_eq!(CredentialStore.get(&t).unwrap().expose_secret(), value);
    }
}

#[test]
fn overwrite_replaces_value() {
    let scope = Scope::new("overwrite");
    let t = scope.target("password");
    CredentialStore.set(&t, &secret("first")).unwrap();
    CredentialStore.set(&t, &secret("second")).unwrap();
    assert_eq!(CredentialStore.get(&t).unwrap().expose_secret(), "second");
    assert_eq!(CredentialStore.list(&scope.0).unwrap(), [t]);
}

#[test]
fn stored_blob_is_encrypted() {
    let scope = Scope::new("stored");
    let t = scope.target("password");
    let value = "plaintext-marker-1234567890";
    CredentialStore.set(&t, &secret(value)).unwrap();

    let blob = read_blob(&t).unwrap();
    assert!(blob.len() > value.len());
    assert!(!contains(&blob, value.as_bytes()));
    let utf16: Vec<u8> = value.encode_utf16().flat_map(u16::to_le_bytes).collect();
    assert!(!contains(&blob, &utf16));

    // 저장 형식: 일반 자격 증명, 이 컴퓨터에만 보관, 사용자 이름 표시
    let name = wide_target(&t).unwrap();
    let mut cred: *mut CREDENTIALW = ptr::null_mut();
    unsafe { CredReadW(PCWSTR(name.as_ptr()), CRED_TYPE_GENERIC, None, &mut cred) }.unwrap();
    let cred = CredBuf(cred);
    let c = unsafe { &*cred.0 };
    assert_eq!(c.Type, CRED_TYPE_GENERIC);
    assert_eq!(c.Persist, CRED_PERSIST_LOCAL_MACHINE);
    assert_eq!(unsafe { pwstr(c.UserName) }, USER_NAME);
    assert_eq!(unsafe { pwstr(c.TargetName) }, t);
}

#[test]
fn blob_is_bound_to_target() {
    let scope = Scope::new("blob");
    let (a, b) = (scope.target("a"), scope.target("b"));
    let value = "bound-secret";
    CredentialStore.set(&a, &secret(value)).unwrap();

    let mut blob = read_blob(&a).unwrap();
    write_blob(&mut wide_target(&b).unwrap(), &mut blob).unwrap();
    let err = CredentialStore.get(&b).unwrap_err();
    assert!(matches!(err, Error::Config(_)), "{err:?}");
    assert!(!err.to_string().contains(value));
    assert_eq!(CredentialStore.get(&a).unwrap().expose_secret(), value);
}

#[test]
fn delete_and_not_found() {
    let scope = Scope::new("delete");
    let t = scope.target("password");
    assert!(matches!(CredentialStore.get(&t), Err(Error::NotFound)));
    assert!(matches!(CredentialStore.delete(&t), Err(Error::NotFound)));

    CredentialStore.set(&t, &secret("x")).unwrap();
    CredentialStore.delete(&t).unwrap();
    assert!(matches!(CredentialStore.get(&t), Err(Error::NotFound)));
}

#[test]
fn lists_by_prefix() {
    let scope = Scope::new("lists");
    let names = [
        scope.target("a/y"),
        scope.target("a/x"),
        scope.target("b/z"),
    ];
    for n in &names {
        CredentialStore.set(n, &secret("v")).unwrap();
    }
    assert_eq!(
        CredentialStore.list(&scope.target("a/")).unwrap(),
        [scope.target("a/x"), scope.target("a/y")]
    );
    assert_eq!(CredentialStore.list(&scope.0).unwrap().len(), 3);
    assert!(
        CredentialStore
            .list(&scope.target("none/"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn secrets_resolves_env_and_credential() {
    let scope = Scope::new("secrets");
    let t = scope.target("password");
    CredentialStore.set(&t, &secret("from-store")).unwrap();
    let got = Secrets
        .resolve(&SecretSource::Credential { target: t.clone() })
        .unwrap();
    assert_eq!(got.expose_secret(), "from-store");

    // PATH는 Windows 프로세스에 항상 있습니다.
    let got = Secrets
        .resolve(&SecretSource::Env { var: "PATH".into() })
        .unwrap();
    assert_eq!(got.expose_secret(), std::env::var("PATH").unwrap());

    let missing = SecretSource::Env {
        var: "EXPANDRV_TEST_MISSING_VAR".into(),
    };
    assert_eq!(
        Secrets.resolve(&missing).unwrap_err().to_string(),
        EnvSecrets.resolve(&missing).unwrap_err().to_string()
    );
}

#[test]
fn rejects_oversize_secret_and_bad_targets() {
    let scope = Scope::new("rejects");
    let t = scope.target("big");
    let max = "가".repeat(MAX_SECRET_BYTES / 3) + &"x".repeat(MAX_SECRET_BYTES % 3);
    assert_eq!(max.len(), MAX_SECRET_BYTES);
    CredentialStore.set(&t, &secret(&max)).unwrap();
    assert_eq!(CredentialStore.get(&t).unwrap().expose_secret(), max);

    let over = "s".repeat(MAX_SECRET_BYTES + 1);
    let err = CredentialStore.set(&t, &secret(&over)).unwrap_err();
    assert!(matches!(err, Error::Config(_)), "{err:?}");
    assert!(!err.to_string().contains(&over));
    // 거부된 값은 기존 값을 바꾸지 않습니다.
    assert_eq!(CredentialStore.get(&t).unwrap().expose_secret(), max);

    let long = "t".repeat(MAX_TARGET_LEN + 1);
    for bad in ["", "a*", "a\0b", long.as_str()] {
        assert!(matches!(
            CredentialStore.set(bad, &secret("v")),
            Err(Error::Config(_))
        ));
        assert!(matches!(CredentialStore.get(bad), Err(Error::Config(_))));
    }
    assert!(matches!(CredentialStore.list(""), Err(Error::Config(_))));
}

#[test]
fn concurrent_set_and_delete_leave_nothing() {
    let scope = Scope::new("concurrent");
    std::thread::scope(|s| {
        for i in 0..8 {
            let t = scope.target(&format!("t{i}"));
            s.spawn(move || {
                for _ in 0..5 {
                    CredentialStore.set(&t, &secret("v")).unwrap();
                    CredentialStore.delete(&t).unwrap();
                }
            });
        }
    });
    assert!(CredentialStore.list(&scope.0).unwrap().is_empty());
}
