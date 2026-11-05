use super::wire::{self, Charset, Fail, PASS_PLACEHOLDER};
use super::*;
use suppaftp::Encoding as _;
use suppaftp::types::Response;

#[test]
fn euc_kr_names_roundtrip() {
    let cs = Charset::new("euc-kr").unwrap();
    // "한글 파일.txt"의 EUC-KR 바이트
    let raw = b"\xc7\xd1\xb1\xdb \xc6\xc4\xc0\xcf.txt";
    assert_eq!(cs.decode(raw), "한글 파일.txt");
    assert_eq!(
        cs.encode("RETR /한글 파일.txt\r\n").unwrap(),
        [b"RETR /".as_slice(), raw, b"\r\n"].concat()
    );
    // EUC-KR로 표현할 수 없는 문자는 보내지 않습니다.
    assert!(cs.encode("RETR /😀\r\n").is_err());
    assert!(!cs.is_utf8());

    let utf8 = Charset::new("UTF-8").unwrap();
    assert!(utf8.is_utf8());
    assert_eq!(utf8.decode("한글".as_bytes()), "한글");
    assert!(Charset::new("utf-16le").is_err());
    assert!(Charset::new("no-such-charset").is_err());
}

#[test]
fn password_only_substituted_on_wire() {
    let cs = Charset::new("utf-8")
        .unwrap()
        .with_password(SecretString::from("s3cr\u{e9}t"));
    let line = cs.encode(&format!("PASS {PASS_PLACEHOLDER}\r\n")).unwrap();
    assert_eq!(line, "PASS s3cr\u{e9}t\r\n".as_bytes());
    // 다른 명령에는 자리표시자를 그대로 둡니다.
    assert_eq!(
        cs.encode(&format!("USER {PASS_PLACEHOLDER}\r\n")).unwrap(),
        format!("USER {PASS_PLACEHOLDER}\r\n").as_bytes()
    );
    assert!(!format!("{cs:?}").contains("s3cr"));
}

fn reply(text: &str) -> Fail {
    let code: u32 = text[..3].parse().unwrap();
    Fail::Ftp(FtpError::UnexpectedResponse(Response::new(
        Status::from(code),
        format!("{text}\r\n").into_bytes(),
    )))
}

#[test]
fn reply_codes_map_to_errors() {
    let map = |t: &str| reply(t).into_error();
    assert!(matches!(
        map("550 No such file or directory."),
        Error::NotFound
    ));
    assert!(matches!(
        map("550 Permission denied."),
        Error::PermissionDenied
    ));
    assert!(matches!(
        map("550 Directory not empty"),
        Error::DirectoryNotEmpty
    ));
    assert!(matches!(map("530 Login incorrect."), Error::Auth(_)));
    assert!(matches!(map("552 Quota exceeded"), Error::StorageFull));
    assert!(matches!(
        map("553 Could not create file."),
        Error::InvalidPath(_)
    ));
    assert!(matches!(map("421 Timeout."), Error::Network(_)));
    assert!(matches!(
        map("554 Invalid REST parameter."),
        Error::Protocol(_)
    ));

    // 응답 코드 오류는 연결을 계속 쓰고, 421과 I/O 오류는 연결을 버립니다.
    assert!(!reply("550 x").is_broken());
    assert_eq!(reply("554 x").code(), Some(554));
    assert!(reply("421 bye").is_broken() && reply("421 bye").is_retryable());
    let io = Fail::Ftp(FtpError::ConnectionError(std::io::Error::from(
        std::io::ErrorKind::ConnectionReset,
    )));
    assert!(io.is_broken() && io.is_retryable());
    assert!(matches!(io.into_error(), Error::Network(_)));
    let timeout = Fail::Ftp(FtpError::ConnectionError(std::io::Error::from(
        std::io::ErrorKind::TimedOut,
    )));
    assert!(matches!(timeout.into_error(), Error::Timeout));
    assert!(Fail::Timeout.is_broken() && !Fail::Timeout.is_retryable());
    assert!(matches!(wire::reply_error(0, "?"), Error::Protocol(_)));
}

#[test]
fn remote_path_helpers() {
    assert_eq!(split_remote("/a/b c"), Some(("/a".to_owned(), "b c")));
    assert_eq!(split_remote("/a"), Some(("/".to_owned(), "a")));
    assert_eq!(split_remote("/"), None);
}

#[test]
fn feats_from_feat_reply() {
    let mut map = suppaftp::types::Features::new();
    for k in ["MLST", "mfmt", "UTF8", "SIZE", "EPSV"] {
        map.insert(k.to_owned(), None);
    }
    let f = Feats::from_map(&map);
    assert!(f.mlst && f.mfmt && f.utf8 && f.size && f.epsv && !f.mdtm);
    assert_eq!(
        Feats::from_map(&suppaftp::types::Features::new()),
        Feats::default()
    );
}
