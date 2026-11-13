use super::xml::{href_path, parse_multistatus};
use super::*;
use expandrv_core::config::SecretSource;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;

const APACHE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:ns0="DAV:">
<D:response xmlns:lp1="DAV:" xmlns:lp2="http://apache.org/dav/props/">
<D:href>/dav/docs/</D:href>
<D:propstat>
<D:prop>
<lp1:resourcetype><D:collection/></lp1:resourcetype>
<lp1:creationdate>2024-01-02T03:04:05Z</lp1:creationdate>
<lp1:getlastmodified>Tue, 02 Jan 2024 03:04:05 GMT</lp1:getlastmodified>
<lp1:getetag>"1000-5f0a"</lp1:getetag>
</D:prop>
<D:status>HTTP/1.1 200 OK</D:status>
</D:propstat>
<D:propstat>
<D:prop><D:getcontentlength/></D:prop>
<D:status>HTTP/1.1 404 Not Found</D:status>
</D:propstat>
</D:response>
<D:response xmlns:lp1="DAV:" xmlns:lp2="http://apache.org/dav/props/">
<D:href>/dav/docs/a%20b%26c.txt</D:href>
<D:propstat>
<D:prop>
<lp1:resourcetype/>
<lp1:creationdate>2024-01-02T12:04:05+09:00</lp1:creationdate>
<lp1:getcontentlength>1234</lp1:getcontentlength>
<lp1:getlastmodified>Tue, 02 Jan 2024 03:04:05 GMT</lp1:getlastmodified>
<lp1:getetag>"4d2-5f0a"</lp1:getetag>
<lp2:executable>F</lp2:executable>
</D:prop>
<D:status>HTTP/1.1 200 OK</D:status>
</D:propstat>
</D:response>
<D:response>
<D:href>/dav/docs/%ED%95%9C%EA%B8%80%20%ED%8F%B4%EB%8D%94/</D:href>
<D:propstat>
<D:prop><lp1:resourcetype xmlns:lp1="DAV:"><D:collection/></lp1:resourcetype></D:prop>
<D:status>HTTP/1.1 200 OK</D:status>
</D:propstat>
</D:response>
</D:multistatus>"#;

/// nginx-dav-ext 형태: 기본 네임스페이스와 접두사 없는 요소
const NGINX: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<multistatus xmlns="DAV:">
<response><href>/files/</href><propstat><prop>
<displayname>files</displayname>
<getlastmodified>Wed, 03 Jan 2024 10:00:00 GMT</getlastmodified>
<resourcetype><collection/></resourcetype>
</prop><status>HTTP/1.1 200 OK</status></propstat></response>
<response><href>/files/r%C3%A9sum%C3%A9.pdf</href><propstat><prop>
<displayname>résumé.pdf</displayname>
<getcontentlength>42</getcontentlength>
<getlastmodified>Wed, 03 Jan 2024 10:00:00 GMT</getlastmodified>
<resourcetype></resourcetype>
</prop><status>HTTP/1.1 200 OK</status></propstat></response>
</multistatus>"#;

/// rclone(golang.org/x/net/webdav) 형태: 빈 resourcetype 시작과 끝 태그
const RCLONE: &str = r#"<?xml version="1.0" encoding="UTF-8"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/</D:href><D:propstat><D:prop><D:resourcetype><D:collection xmlns:D="DAV:"/></D:resourcetype><D:getlastmodified>Thu, 04 Jan 2024 00:00:00 GMT</D:getlastmodified><D:displayname></D:displayname></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><D:response><D:href>/x%20y.bin</D:href><D:propstat><D:prop><D:resourcetype></D:resourcetype><D:getcontentlength>5</D:getcontentlength><D:getetag>"17a8b"</D:getetag><D:getlastmodified>Thu, 04 Jan 2024 00:00:00 GMT</D:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;

/// Nextcloud(sabre/dav) 형태: `d:` 접두사, 절대 URL href, 엔티티 이스케이프, 음수 할당량
const NEXTCLOUD: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns">
<d:response>
<d:href>https://cloud.example.com/remote.php/dav/files/user%20name/Docs/</d:href>
<d:propstat><d:prop>
<d:resourcetype><d:collection/></d:resourcetype>
<d:quota-used-bytes>123456</d:quota-used-bytes>
<d:quota-available-bytes>-3</d:quota-available-bytes>
<d:getetag>&quot;65a1b2c3d4e5f&quot;</d:getetag>
<d:current-user-principal><d:href>/remote.php/dav/principals/users/u/</d:href></d:current-user-principal>
<oc:size>999</oc:size>
</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
<d:propstat><d:prop><d:getcontentlength/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
</d:response>
<d:response>
<d:href>https://cloud.example.com/remote.php/dav/files/user%20name/Docs/Q%26A%20%231.md</d:href>
<d:propstat><d:prop>
<d:resourcetype/>
<d:getcontentlength>7</d:getcontentlength>
<d:getlastmodified>Fri, 05 Jan 2024 01:02:03 GMT</d:getlastmodified>
<d:getetag>&quot;abc&quot;</d:getetag>
</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
</d:response>
<d:response>
<d:href><![CDATA[/remote.php/dav/files/user%20name/Docs/gone.txt]]></d:href>
<d:status>HTTP/1.1 404 Not Found</d:status>
</d:response>
</d:multistatus>"#;

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(secs)
}

#[test]
fn parses_apache_mod_dav() {
    let r = parse_multistatus(APACHE).unwrap();
    assert_eq!(r.len(), 3);
    assert_eq!(r[0].href, "/dav/docs/");
    assert!(r[0].props.collection);
    assert_eq!(r[0].props.content_length, None);
    assert_eq!(r[0].props.creation_date, Some(at(1_704_164_645)));
    assert_eq!(r[0].props.last_modified, Some(at(1_704_164_645)));
    assert_eq!(r[0].props.etag.as_deref(), Some("\"1000-5f0a\""));
    assert!(!r[1].props.collection);
    assert_eq!(r[1].props.content_length, Some(1234));
    // +09:00 오프셋을 UTC로 환산
    assert_eq!(r[1].props.creation_date, Some(at(1_704_164_645)));
    assert!(r[2].props.collection);
}

#[test]
fn parses_default_namespace_nginx() {
    let r = parse_multistatus(NGINX).unwrap();
    assert_eq!(r.len(), 2);
    assert!(r[0].props.collection);
    assert_eq!(r[1].props.content_length, Some(42));
    assert!(!r[1].props.collection);
    assert_eq!(href_path(&r[1].href, "/files"), "/files/résumé.pdf");
}

#[test]
fn parses_rclone() {
    let r = parse_multistatus(RCLONE).unwrap();
    assert_eq!(r.len(), 2);
    assert!(r[0].props.collection);
    assert!(!r[1].props.collection);
    assert_eq!(r[1].props.content_length, Some(5));
    assert_eq!(r[1].props.etag.as_deref(), Some("\"17a8b\""));
}

#[test]
fn parses_nextcloud_prefix_entities_and_quota() {
    let r = parse_multistatus(NEXTCLOUD).unwrap();
    assert_eq!(r.len(), 3);
    // prop 안의 href(current-user-principal)는 응답 href를 덮어쓰지 않습니다.
    assert!(r[0].href.ends_with("/Docs/"));
    assert_eq!(r[0].props.quota_used, Some(123_456));
    assert_eq!(r[0].props.quota_available, None);
    assert_eq!(r[0].props.etag.as_deref(), Some("\"65a1b2c3d4e5f\""));
    assert_eq!(r[0].props.content_length, None);
    assert_eq!(r[1].props.content_length, Some(7));
    assert!(r[0].ok && r[1].ok);
    assert!(!r[2].ok);
    assert_eq!(
        href_path(&r[2].href, "/"),
        "/remote.php/dav/files/user name/Docs/gone.txt"
    );
}

#[test]
fn rejects_malformed_xml() {
    assert!(parse_multistatus("<D:multistatus xmlns:D=\"DAV:\"><D:response></D:x>").is_err());
}

#[test]
fn decodes_hrefs() {
    assert_eq!(href_path("/a%20b/%ED%95%9C.txt", "/"), "/a b/한.txt");
    assert_eq!(
        href_path("https://h.example.com:8443/dav/x%23y", "/"),
        "/dav/x#y"
    );
    assert_eq!(href_path("http://h.example.com", "/"), "/");
    assert_eq!(href_path("/dav/a?query=1", "/"), "/dav/a");
    assert_eq!(href_path("rel%20name", "/dav/dir/"), "/dav/dir/rel name");
}

fn resp(href: &str, collection: bool) -> DavResponse {
    DavResponse {
        href: href.to_owned(),
        ok: true,
        props: Props {
            collection,
            ..Props::default()
        },
    }
}

#[test]
fn split_self_skips_self_and_deeper_entries() {
    let (me, kids) = split_self(
        "/dav/docs/",
        vec![
            resp("/dav/docs/a.txt", false),
            resp("https://h.example.com/dav/docs/", true),
            resp("/dav/docs/sub/", true),
            resp("/dav/docs/sub/deep.txt", false),
        ],
    );
    assert!(me.unwrap().props.collection);
    let names: Vec<_> = kids.iter().map(|(p, _)| xml::last_segment(p)).collect();
    assert_eq!(names, ["a.txt", "sub"]);
}

#[test]
fn split_self_falls_back_to_shortest_href() {
    // 프록시가 접두를 바꿔 요청 경로와 일치하는 href가 없는 경우
    let (me, kids) = split_self(
        "/public/dav/",
        vec![resp("/dav/a%20b", false), resp("/dav/", true)],
    );
    assert_eq!(me.unwrap().href, "/dav/");
    assert_eq!(xml::last_segment(&kids[0].0), "a b");
}

struct Fixed;

impl SecretResolver for Fixed {
    fn resolve(&self, _: &SecretSource) -> Result<SecretString> {
        Ok(SecretString::from("s3cr3t-pw"))
    }
}

fn cfg(url: &str) -> WebDavConfig {
    WebDavConfig {
        url: url.to_owned(),
        username: Some("user".into()),
        password: Some(SecretSource::Env {
            var: "UNUSED".into(),
        }),
        pinned_cert_sha256: None,
    }
}

fn p(s: &str) -> RemotePath {
    RemotePath::parse(s).unwrap()
}

#[test]
fn builds_percent_encoded_urls() {
    let be = WebDavBackend::new(&cfg("https://dav.example.com/root dir?x=1"), &Fixed).unwrap();
    assert_eq!(be.base.as_str(), "https://dav.example.com/root%20dir/");
    assert_eq!(
        be.url(&p("/한글 폴더/a#b+c.txt"), false).unwrap().as_str(),
        "https://dav.example.com/root%20dir/%ED%95%9C%EA%B8%80%20%ED%8F%B4%EB%8D%94/a%23b%2Bc.txt"
    );
    assert_eq!(
        be.url(&p("/d"), true).unwrap().as_str(),
        "https://dav.example.com/root%20dir/d/"
    );
    assert_eq!(
        be.url(&RemotePath::root(), true).unwrap().as_str(),
        "https://dav.example.com/root%20dir/"
    );
}

#[test]
fn rejects_bad_config_without_leaking_secrets() {
    let e = WebDavBackend::new(&cfg("https://u:hunter2@dav.example.com/"), &Fixed)
        .err()
        .unwrap();
    assert!(matches!(e, Error::Config(_)));
    assert!(!e.to_string().contains("hunter2"));
    assert!(WebDavBackend::new(&cfg("ftp://dav.example.com/"), &Fixed).is_err());
    let mut c = cfg("http://dav.example.com/");
    c.pinned_cert_sha256 = Some("00".repeat(32));
    assert!(WebDavBackend::new(&c, &Fixed).is_err());
    let mut c = cfg("https://dav.example.com/");
    c.pinned_cert_sha256 = Some("zz".into());
    assert!(WebDavBackend::new(&c, &Fixed).is_err());
    c.pinned_cert_sha256 = Some("AB:".repeat(32));
    assert!(WebDavBackend::new(&c, &Fixed).is_ok());
}

#[test]
fn maps_http_status() {
    let s = |c: u16| StatusCode::from_u16(c).unwrap();
    assert!(matches!(status_error(s(404), Op::Get), Error::NotFound));
    assert!(matches!(status_error(s(401), Op::Get), Error::Auth(_)));
    assert!(matches!(
        status_error(s(403), Op::Put),
        Error::PermissionDenied
    ));
    assert!(matches!(
        status_error(s(405), Op::Mkcol),
        Error::AlreadyExists
    ));
    assert!(matches!(status_error(s(405), Op::Put), Error::Protocol(_)));
    assert!(matches!(status_error(s(409), Op::Put), Error::NotFound));
    assert!(matches!(
        status_error(s(412), Op::Move),
        Error::AlreadyExists
    ));
    assert!(matches!(status_error(s(507), Op::Put), Error::StorageFull));
    assert!(matches!(status_error(s(504), Op::Get), Error::Timeout));
    assert!(matches!(status_error(s(503), Op::Get), Error::Network(_)));
}

#[test]
fn parses_content_range() {
    let mut h = HeaderMap::new();
    h.insert(CONTENT_RANGE, "bytes 100-199/1000".parse().unwrap());
    assert_eq!(content_range_start(&h), Some(100));
    h.insert(CONTENT_RANGE, "bytes */1000".parse().unwrap());
    assert_eq!(content_range_start(&h), None);
}

#[test]
fn pinned_verifier_checks_leaf_digest() {
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use sha2::{Digest, Sha256};

    let der = b"not-a-real-certificate".to_vec();
    let hex: String = Sha256::digest(&der)
        .iter()
        .map(|b| format!("{b:02X}:"))
        .collect();
    let pin = tls::parse_fingerprint(&hex).unwrap();
    let provider = rustls::crypto::ring::default_provider();
    let v = tls::PinnedVerifier::new(pin, &provider);
    let name = ServerName::try_from("dav.example.com").unwrap();
    let cert = CertificateDer::from(der);
    assert!(
        v.verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
            .is_ok()
    );
    let other = CertificateDer::from(b"other".to_vec());
    assert!(
        v.verify_server_cert(&other, &[], &name, &[], UnixTime::now())
            .is_err()
    );
    assert!(!v.supported_verify_schemes().is_empty());
    assert!(tls::pinned_client_config(pin).is_ok());
}

/// 연결마다 요청 하나를 받아 준비된 (상태 줄, 본문) 응답을 차례로 돌려주는 HTTP/1.1 모의 서버
async fn mock(
    responses: Vec<(&'static str, &'static str)>,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/dav/", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let mut seen = Vec::new();
        for (status, body) in responses {
            let (mut sock, _) = listener.accept().await.unwrap();
            seen.push(read_request(&mut sock).await);
            let msg = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(msg.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
        }
        seen
    });
    (url, handle)
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let head_end = loop {
        let n = sock.read(&mut tmp).await.unwrap();
        assert!(n > 0, "요청 헤더 전에 연결이 끊겼습니다");
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
    let clen = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .map(|v| v.trim().parse::<usize>().unwrap());
    let chunked = head.contains("transfer-encoding: chunked");
    loop {
        let body = &buf[head_end..];
        let done = match clen {
            Some(n) => body.len() >= n,
            None if chunked => body.ends_with(b"0\r\n\r\n"),
            None => true,
        };
        if done {
            break;
        }
        let n = sock.read(&mut tmp).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    format!(
        "{}body-len={}",
        String::from_utf8_lossy(&buf[..head_end]),
        buf.len() - head_end
    )
}

const ROOT_207: (&str, &str) = (
    "207 Multi-Status",
    r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/dav/</D:href><D:propstat><D:prop><D:resourcetype><D:collection/></D:resourcetype></D:prop></D:propstat></D:response></D:multistatus>"#,
);

#[tokio::test]
async fn connect_maps_401_to_auth() {
    let (url, h) = mock(vec![("401 Unauthorized", "")]).await;
    let e = WebDavBackend::connect(&cfg(&url), &Fixed)
        .await
        .err()
        .unwrap();
    assert!(matches!(e, Error::Auth(_)), "{e:?}");
    assert!(!e.to_string().contains("s3cr3t"));
    let reqs = h.await.unwrap();
    assert!(reqs[0].starts_with("PROPFIND /dav/ HTTP/1.1"));
    assert!(
        reqs[0]
            .to_ascii_lowercase()
            .contains("authorization: basic")
    );
    assert!(reqs[0].to_ascii_lowercase().contains("depth: 0"));
}

#[tokio::test]
async fn read_range_slices_full_body_and_handles_416() {
    let (url, h) = mock(vec![
        ROOT_207,
        ("200 OK", "0123456789"),
        ("416 Range Not Satisfiable", ""),
    ])
    .await;
    let be = WebDavBackend::connect(&cfg(&url), &Fixed).await.unwrap();
    assert_eq!(&be.read_range(&p("/f"), 3, 4).await.unwrap()[..], b"3456");
    assert!(be.read_range(&p("/f"), 50, 4).await.unwrap().is_empty());
    let reqs = h.await.unwrap();
    assert!(reqs[1].to_ascii_lowercase().contains("range: bytes=3-6"));
}

#[tokio::test]
async fn put_from_file_sends_content_length() {
    const STAT_207: (&str, &str) = (
        "207 Multi-Status",
        r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/dav/up.bin</D:href><D:propstat><D:prop><D:getcontentlength>300000</D:getcontentlength></D:prop></D:propstat></D:response></D:multistatus>"#,
    );
    let (url, h) = mock(vec![ROOT_207, ("201 Created", ""), STAT_207]).await;
    let be = WebDavBackend::connect(&cfg(&url), &Fixed).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("up.bin");
    std::fs::write(&src, vec![9u8; 300_000]).unwrap();
    let e = be
        .write(
            &p("/up.bin"),
            WriteSource::File {
                path: src,
                size: 300_000,
            },
        )
        .await
        .unwrap();
    assert_eq!((e.name.as_str(), e.size), ("up.bin", 300_000));
    let reqs = h.await.unwrap();
    let put = reqs[1].to_ascii_lowercase();
    assert!(put.starts_with("put /dav/up.bin http/1.1"));
    assert!(put.contains("content-length: 300000"));
    assert!(!put.contains("transfer-encoding"));
    assert!(put.ends_with("body-len=300000"));
}

#[tokio::test]
async fn streaming_put_with_size_hint_sends_content_length() {
    const STAT_207: (&str, &str) = (
        "207 Multi-Status",
        r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/dav/s.bin</D:href><D:propstat><D:prop><D:getcontentlength>300000</D:getcontentlength></D:prop></D:propstat></D:response></D:multistatus>"#,
    );
    let (url, h) = mock(vec![ROOT_207, ("201 Created", ""), STAT_207]).await;
    let be = WebDavBackend::connect(&cfg(&url), &Fixed).await.unwrap();
    let mut s = be
        .begin_upload(
            &p("/s.bin"),
            UploadHint {
                size: Some(300_000),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    s.write(0, Bytes::from(vec![1u8; 100_000])).await.unwrap();
    s.flush().await.unwrap();
    s.write(100_000, Bytes::from(vec![2u8; 200_000]))
        .await
        .unwrap();
    let e = s.finish().await.unwrap();
    assert_eq!((e.name.as_str(), e.size), ("s.bin", 300_000));
    let reqs = h.await.unwrap();
    let put = reqs[1].to_ascii_lowercase();
    assert!(put.starts_with("put /dav/s.bin http/1.1"));
    assert!(put.contains("content-length: 300000"));
    assert!(!put.contains("transfer-encoding"));
    assert!(put.ends_with("body-len=300000"));
}

#[tokio::test]
async fn streaming_put_without_size_hint_uses_chunked_body() {
    const STAT_207: (&str, &str) = (
        "207 Multi-Status",
        r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/dav/c.bin</D:href><D:propstat><D:prop><D:getcontentlength>7</D:getcontentlength></D:prop></D:propstat></D:response></D:multistatus>"#,
    );
    let (url, h) = mock(vec![ROOT_207, ("201 Created", ""), STAT_207]).await;
    let be = WebDavBackend::connect(&cfg(&url), &Fixed).await.unwrap();
    let mut s = be
        .begin_upload(&p("/c.bin"), UploadHint::default())
        .await
        .unwrap();
    s.write(0, Bytes::from_static(b"abc")).await.unwrap();
    s.write(3, Bytes::from_static(b"defg")).await.unwrap();
    assert!(matches!(
        s.write(0, Bytes::from_static(b"x")).await,
        Err(Error::Unsupported(_))
    ));
    let e = s.finish().await.unwrap();
    assert_eq!(e.size, 7);
    let reqs = h.await.unwrap();
    let put = reqs[1].to_ascii_lowercase();
    assert!(put.contains("transfer-encoding: chunked"), "{put}");
    assert!(!put.contains("content-length"));
}

#[tokio::test]
async fn streaming_put_size_mismatch_and_rejection_are_errors() {
    let (url, h) = mock(vec![ROOT_207, ("507 Insufficient Storage", "")]).await;
    let be = WebDavBackend::connect(&cfg(&url), &Fixed).await.unwrap();
    // 미리 알린 크기보다 적게 보내면 확정하지 않습니다.
    let mut s = be
        .begin_upload(
            &p("/m.bin"),
            UploadHint {
                size: Some(10),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    s.write(0, Bytes::from_static(b"abc")).await.unwrap();
    assert!(matches!(s.finish().await, Err(Error::Protocol(_))));
    // 서버가 거부하면 그 상태가 오류로 돌아옵니다.
    let mut s = be
        .begin_upload(
            &p("/r.bin"),
            UploadHint {
                size: Some(3),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    s.write(0, Bytes::from_static(b"abc")).await.unwrap();
    assert!(matches!(s.finish().await, Err(Error::StorageFull)));
    h.abort();
}

#[tokio::test]
async fn mkcol_405_and_move_412_map_to_already_exists() {
    let (url, h) = mock(vec![
        ROOT_207,
        ("404 Not Found", ""),
        ("405 Method Not Allowed", ""),
        ("412 Precondition Failed", ""),
    ])
    .await;
    let be = WebDavBackend::connect(&cfg(&url), &Fixed).await.unwrap();
    assert!(matches!(
        be.mkdir(&p("/d")).await,
        Err(Error::AlreadyExists)
    ));
    assert!(matches!(
        be.rename(&p("/a b"), &p("/c"), false).await,
        Err(Error::AlreadyExists)
    ));
    let reqs = h.await.unwrap();
    assert!(reqs[1].starts_with("PROPFIND /dav/d HTTP/1.1"));
    assert!(reqs[2].starts_with("MKCOL /dav/d/ HTTP/1.1"));
    let mv = reqs[3].to_ascii_lowercase();
    assert!(mv.starts_with("move /dav/a%20b http/1.1"));
    assert!(mv.contains(&format!("destination: {}c", url.to_ascii_lowercase())));
    assert!(mv.contains("overwrite: f"));
}
