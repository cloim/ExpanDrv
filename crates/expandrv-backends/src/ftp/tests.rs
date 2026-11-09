use super::wire::{self, Charset, Fail, PASS_PLACEHOLDER};
use super::*;
use suppaftp::Encoding as _;
use suppaftp::types::Response;

#[tokio::test]
async fn ftps_pinning_accepts_only_confirmed_certificate() {
    use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName};
    use sha2::{Digest, Sha256};
    use suppaftp::tokio_rustls::TlsAcceptor;
    use tokio::net::{TcpListener, TcpStream};

    // 로컬 테스트 전용 자체 서명 인증서와 키이며 실제 서버 자격 증명이 아닙니다.
    let cert = include_bytes!("../../tests/fixtures/ftps-localhost.der");
    let key = include_bytes!("../../tests/fixtures/ftps-localhost-key.der");
    let pin = format!("{:x}", Sha256::digest(cert));
    for tls in [FtpTls::Explicit, FtpTls::Implicit] {
        for (configured_pin, expected) in [
            (None, false),
            (Some("00".repeat(32)), false),
            (Some(pin.clone()), true),
        ] {
            let mut cfg: FtpConfig = toml_config_for_tls_test();
            cfg.tls = tls;
            cfg.pinned_cert_sha256 = configured_pin;
            let client_config = configured_tls(&cfg).unwrap();
            assert!(
                client_config.alpn_protocols.is_empty(),
                "FTPS에 HTTP ALPN을 보내지 않습니다"
            );
            let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(cert.to_vec())],
                PrivatePkcs8KeyDer::from(key.to_vec()).into(),
            )
            .unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let acceptor = TlsAcceptor::from(Arc::new(server_config));
                if let Ok(mut tls) = acceptor.accept(stream).await {
                    let _ = tls.write_all(b"X").await;
                }
            });
            let result = tokio::time::timeout(Duration::from_secs(5), async {
                let stream = TcpStream::connect(addr).await.unwrap();
                let mut stream = TlsConnector::from(client_config)
                    .connect(ServerName::try_from("localhost").unwrap(), stream)
                    .await?;
                let mut byte = [0];
                stream.read_exact(&mut byte).await?;
                assert_eq!(byte, [b'X']);
                Ok::<_, std::io::Error>(())
            })
            .await
            .unwrap();
            assert_eq!(result.is_ok(), expected, "TLS {tls:?}: {result:?}");
            tokio::time::timeout(Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap();
        }
    }
}

fn toml_config_for_tls_test() -> FtpConfig {
    FtpConfig {
        host: "localhost".into(),
        port: 21,
        username: "anonymous".into(),
        password: None,
        tls: FtpTls::Explicit,
        pinned_cert_sha256: None,
        encoding: "utf-8".into(),
        root: "/".into(),
    }
}

#[test]
fn ftps_rejects_invalid_pin_and_plaintext_pin() {
    let mut cfg = toml_config_for_tls_test();
    cfg.pinned_cert_sha256 = Some("invalid".into());
    assert!(configured_tls(&cfg).is_err());
    cfg.pinned_cert_sha256 = Some("00".repeat(32));
    cfg.tls = FtpTls::None;
    assert!(configured_tls(&cfg).is_err());
}

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

#[tokio::test(start_paused = true)]
async fn read_until_stall_reports_end_more_data_and_stall() {
    use tokio::io::AsyncWriteExt;
    let (mut tx, mut rx) = tokio::io::duplex(64);
    tx.write_all(b"0123456789").await.unwrap();
    drop(tx);
    let mut buf = Vec::new();
    assert_eq!(
        read_until_stall(&mut rx, 20, &mut buf).await.unwrap(),
        Tail::Eof
    );
    assert_eq!(buf, b"0123456789");

    let (mut tx, mut rx) = tokio::io::duplex(64);
    tx.write_all(b"0123456789").await.unwrap();
    let mut buf = Vec::new();
    assert_eq!(
        read_until_stall(&mut rx, 4, &mut buf).await.unwrap(),
        Tail::More(Some(b'4'))
    );
    assert_eq!(buf, b"0123");

    // 서버가 연결을 연 채 데이터를 보내지 않으면 제한 시간 뒤 멈춤으로 알립니다.
    let (mut tx, mut rx) = tokio::io::duplex(64);
    tx.write_all(b"abc").await.unwrap();
    let mut buf = Vec::new();
    let started = tokio::time::Instant::now();
    assert_eq!(
        read_until_stall(&mut rx, 10, &mut buf).await.unwrap(),
        Tail::Stalled
    );
    assert_eq!(started.elapsed(), STALL_TIMEOUT);
    assert_eq!(buf, b"abc");
    drop(tx);
}
