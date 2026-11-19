use super::*;
use expandrv_core::config::SecretSource;
use std::sync::Mutex as StdMutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn p(s: &str) -> RemotePath {
    RemotePath::parse(s).unwrap()
}

fn file(id: &str, name: &str, mime: &str) -> DriveFile {
    DriveFile {
        id: id.into(),
        name: name.into(),
        mime_type: mime.into(),
        size: Some("12".into()),
        modified_time: Some("2026-11-19T01:02:03.456Z".into()),
        created_time: None,
        md5_checksum: Some("abc".into()),
        version: Some("7".into()),
        capabilities: None,
    }
}

#[test]
fn entries_hide_workspace_files_and_invalid_names() {
    let doc = file("1", "보고서", "application/vnd.google-apps.document");
    assert!(doc.entry().is_none());
    let shortcut = file("2", "link", "application/vnd.google-apps.shortcut");
    assert!(shortcut.entry().is_none());
    assert!(file("3", "a/b.txt", "text/plain").entry().is_none());
    assert!(file("4", "what?.txt", "text/plain").entry().is_none());

    let folder = file("5", "사진", FOLDER_MIME).entry().unwrap();
    assert!(folder.is_dir());
    assert_eq!(folder.etag, None);

    let mut f = file("6", "한글 & 메모.txt", "text/plain");
    f.capabilities = Some(FileCapabilities {
        can_edit: Some(false),
    });
    let e = f.entry().unwrap();
    assert_eq!(e.size, 12);
    assert_eq!(e.etag.as_deref(), Some("abc"));
    assert!(e.readonly);
    assert!(e.modified.is_some());
}

#[test]
fn duplicate_names_keep_the_first_listed() {
    let mut newer = file("new", "a.txt", "text/plain");
    newer.size = Some("2".into());
    let older = file("old", "a.txt", "text/plain");
    let list = visible_entries(vec![newer, older, file("d", "docs", FOLDER_MIME)]);
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].0.id, "new");
    assert_eq!(list[0].1.size, 2);

    // 더 최근의 Google 문서가 같은 이름이어도 일반 파일은 보입니다.
    let doc = file("doc", "a.txt", "application/vnd.google-apps.document");
    let list = visible_entries(vec![doc, file("bin", "a.txt", "text/plain")]);
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].0.id, "bin");
}

#[test]
fn query_values_are_escaped() {
    assert_eq!(quote(r"it's a\b"), r"'it\'s a\\b'");
}

#[test]
fn ids_are_checked_before_use_in_urls() {
    assert_eq!(check_id("ID", " 0AbC-_9 ").unwrap(), "0AbC-_9");
    for bad in ["", "a/b", "a?b", "../x", "a b"] {
        assert!(check_id("ID", bad).is_err(), "{bad}");
    }
}

#[test]
fn api_errors_map_to_file_system_errors() {
    let body = |reason: &str| {
        format!(r#"{{"error":{{"code":403,"message":"m","errors":[{{"reason":"{reason}"}}]}}}}"#)
    };
    assert!(matches!(api_error(404, ""), (false, Error::NotFound)));
    assert!(matches!(
        api_error(403, &body("storageQuotaExceeded")),
        (false, Error::StorageFull)
    ));
    assert!(matches!(
        api_error(403, &body("userRateLimitExceeded")),
        (true, Error::Network(_))
    ));
    assert!(matches!(
        api_error(403, &body("accessNotConfigured")),
        (false, Error::Config(_))
    ));
    assert!(matches!(
        api_error(403, &body("insufficientFilePermissions")),
        (false, Error::PermissionDenied)
    ));
    assert!(matches!(api_error(503, "x"), (true, Error::Network(_))));
    assert!(matches!(api_error(401, ""), (false, Error::Auth(_))));
}

#[test]
fn token_errors_explain_the_next_step() {
    let e = oauth::token_error(
        400,
        r#"{"error":"invalid_grant","error_description":"Bad"}"#,
    );
    assert!(
        matches!(&e, Error::Auth(m) if m.contains("다시 로그인")),
        "{e}"
    );
    let e = oauth::token_error(401, r#"{"error":"invalid_client"}"#);
    assert!(
        matches!(&e, Error::Auth(m) if m.contains("클라이언트")),
        "{e}"
    );
    assert!(matches!(
        oauth::token_error(502, "<html>"),
        Error::Network(_)
    ));
}

#[test]
fn pkce_and_authorization_url() {
    // RFC 7636 부록 B 예시
    assert_eq!(
        oauth::code_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    let a = oauth::random_token(32).unwrap();
    assert_eq!(a.len(), 43);
    assert_ne!(a, oauth::random_token(32).unwrap());
    let url = oauth::authorization_url(
        oauth::AUTH_URL,
        "cid.apps.googleusercontent.com",
        "http://127.0.0.1:5000",
        "chal",
        "st",
    )
    .unwrap();
    let url = Url::parse(&url).unwrap();
    let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(q["redirect_uri"], "http://127.0.0.1:5000");
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["access_type"], "offline");
    assert_eq!(q["scope"], oauth::SCOPE);
}

#[test]
fn callback_requires_matching_state() {
    use oauth::{Callback, parse_callback};
    assert_eq!(
        parse_callback("GET /?state=s1&code=4%2Fabc HTTP/1.1", "s1"),
        Callback::Code("4/abc".into())
    );
    assert_eq!(
        parse_callback("GET /?state=other&code=x HTTP/1.1", "s1"),
        Callback::Ignore
    );
    assert_eq!(
        parse_callback("GET /favicon.ico HTTP/1.1", "s1"),
        Callback::Ignore
    );
    assert_eq!(
        parse_callback("GET /?state=s1&error=access_denied HTTP/1.1", "s1"),
        Callback::Denied("access_denied".into())
    );
    assert_eq!(
        parse_callback("POST /?state=s1&code=x HTTP/1.1", "s1"),
        Callback::Ignore
    );
}

// ---- 모의 Google 서버 ----

#[derive(Clone)]
struct MockFile {
    id: String,
    name: String,
    parent: String,
    folder: bool,
    /// Google 문서처럼 내려받을 내용이 없는 Workspace 파일
    doc: bool,
    data: Vec<u8>,
    modified: String,
    trashed: bool,
}

struct Session {
    target: Option<String>,
    name: String,
    parent: String,
    data: Vec<u8>,
}

#[derive(Default)]
struct MockState {
    files: Vec<MockFile>,
    next: u32,
    sessions: HashMap<String, Session>,
    token_calls: u32,
    /// 다음 API 요청 하나를 401로 거부합니다(토큰 만료 흉내).
    reject_next: bool,
    log: Vec<String>,
    base: String,
}

struct Req {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn json_resp(status: u16, v: serde_json::Value) -> Resp {
    Resp {
        status,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: serde_json::to_vec(&v).unwrap(),
    }
}

impl MockState {
    fn add(&mut self, name: &str, parent: &str, folder: bool, data: &[u8]) -> String {
        self.next += 1;
        let id = format!("f{}", self.next);
        self.files.push(MockFile {
            id: id.clone(),
            name: name.into(),
            parent: parent.into(),
            folder,
            doc: false,
            data: data.to_vec(),
            modified: format!("2026-11-19T00:00:{:02}.000Z", self.next % 60),
            trashed: false,
        });
        id
    }

    fn meta(f: &MockFile) -> serde_json::Value {
        let mut v = serde_json::json!({
            "id": f.id, "name": f.name, "modifiedTime": f.modified,
            "mimeType": if f.folder {
                FOLDER_MIME
            } else if f.doc {
                "application/vnd.google-apps.document"
            } else {
                "application/octet-stream"
            },
        });
        if !f.folder && !f.doc {
            v["size"] = f.data.len().to_string().into();
            v["md5Checksum"] = format!("len{}", f.data.len()).into();
        }
        v
    }

    fn find(&mut self, id: &str) -> Option<&mut MockFile> {
        self.files.iter_mut().find(|f| f.id == id && !f.trashed)
    }

    fn handle(&mut self, r: Req) -> Resp {
        self.log.push(format!("{} {}", r.method, r.path));
        if r.path == "/token" {
            self.token_calls += 1;
            let body = String::from_utf8_lossy(&r.body).to_string();
            assert!(body.contains("grant_type=refresh_token"), "{body}");
            assert!(body.contains("client_secret=cs"), "{body}");
            if !body.contains("refresh_token=rt") {
                return json_resp(400, serde_json::json!({"error": "invalid_grant"}));
            }
            return json_resp(
                200,
                serde_json::json!({"access_token": format!("at{}", self.token_calls), "expires_in": 3600}),
            );
        }
        let auth = r.headers.get("authorization").cloned().unwrap_or_default();
        if self.reject_next || !auth.starts_with("Bearer at") {
            self.reject_next = false;
            return json_resp(
                401,
                serde_json::json!({"error": {"code": 401, "message": "expired"}}),
            );
        }
        if r.path.starts_with("/upload/session/") {
            return self.session_put(&r);
        }
        let seg: Vec<&str> = r.path.trim_start_matches('/').split('/').collect();
        match (r.method.as_str(), seg.as_slice()) {
            ("GET", ["drive", "v3", "about"]) => json_resp(
                200,
                serde_json::json!({"user": {"emailAddress": "me@example.com"},
                    "storageQuota": {"limit": "1000", "usage": "250"}}),
            ),
            ("GET", ["drive", "v3", "files"]) => self.list(&r),
            ("POST", ["drive", "v3", "files"]) => {
                let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
                let parent = v["parents"][0].as_str().unwrap().to_owned();
                let id = self.add(v["name"].as_str().unwrap(), &parent, true, b"");
                let f = self.find(&id).unwrap().clone();
                json_resp(200, Self::meta(&f))
            }
            ("GET", ["drive", "v3", "files", id]) => {
                let id = id.to_string();
                let Some(f) = self.find(&id).cloned() else {
                    return json_resp(
                        404,
                        serde_json::json!({"error": {"code": 404, "message": "nf"}}),
                    );
                };
                if r.query.get("alt").map(String::as_str) == Some("media") {
                    let range = r.headers.get("range").cloned().unwrap_or_default();
                    let (a, b) = range
                        .trim_start_matches("bytes=")
                        .split_once('-')
                        .map(|(a, b)| (a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap()))
                        .unwrap();
                    let b = b.min(f.data.len() - 1);
                    return Resp {
                        status: 206,
                        headers: vec![],
                        body: f.data[a..=b].to_vec(),
                    };
                }
                json_resp(200, Self::meta(&f))
            }
            ("PATCH", ["drive", "v3", "files", id]) => {
                let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
                let (id, q) = (id.to_string(), r.query.clone());
                let Some(f) = self.find(&id) else {
                    return json_resp(
                        404,
                        serde_json::json!({"error": {"code": 404, "message": "nf"}}),
                    );
                };
                if let Some(n) = v["name"].as_str() {
                    f.name = n.into();
                }
                if v["trashed"].as_bool() == Some(true) {
                    f.trashed = true;
                }
                if let Some(m) = v["modifiedTime"].as_str() {
                    f.modified = m.into();
                }
                if let Some(add) = q.get("addParents") {
                    assert_eq!(Some(&f.parent), q.get("removeParents"));
                    f.parent = add.clone();
                }
                json_resp(200, serde_json::json!({"id": id}))
            }
            (m, ["upload", "drive", "v3", "files", rest @ ..]) => {
                self.upload(m, rest.first().copied(), &r)
            }
            _ => json_resp(
                400,
                serde_json::json!({"error": {"code": 400, "message": "unexpected"}}),
            ),
        }
    }

    fn list(&mut self, r: &Req) -> Resp {
        let q = &r.query["q"];
        let parent = q.split('\'').nth(1).unwrap().to_owned();
        let name = q.split_once("name = '").map(|(_, rest)| {
            rest.rsplit_once("' and")
                .unwrap()
                .0
                .replace("\\'", "'")
                .replace("\\\\", "\\")
        });
        let mut hits: Vec<&MockFile> = self
            .files
            .iter()
            .filter(|f| !f.trashed && f.parent == parent)
            .filter(|f| name.as_ref().is_none_or(|n| *n == f.name))
            .collect();
        hits.sort_by(|a, b| b.modified.cmp(&a.modified));
        let size: usize = r.query["pageSize"].parse().unwrap();
        let start: usize = r.query.get("pageToken").map_or(0, |t| t.parse().unwrap());
        let page: Vec<_> = hits
            .iter()
            .skip(start)
            .take(size.min(2))
            .map(|f| Self::meta(f))
            .collect();
        let mut v = serde_json::json!({ "files": page });
        if start + 2 < hits.len() && size > 2 {
            v["nextPageToken"] = (start + 2).to_string().into();
        }
        json_resp(200, v)
    }

    fn upload(&mut self, method: &str, id: Option<&str>, r: &Req) -> Resp {
        let kind = r.query["uploadType"].clone();
        let (meta, data) = if kind == "multipart" {
            let ct = &r.headers["content-type"];
            let boundary = ct.split("boundary=").nth(1).unwrap();
            let text = r.body.clone();
            let sep = format!("--{boundary}");
            let parts = split_bytes(&text, sep.as_bytes());
            let meta_part = &parts[1];
            let data_part = &parts[2];
            let meta_json =
                &meta_part[find(meta_part, b"\r\n\r\n").unwrap() + 4..meta_part.len() - 2];
            let data = &data_part[find(data_part, b"\r\n\r\n").unwrap() + 4..data_part.len() - 2];
            (
                serde_json::from_slice::<serde_json::Value>(meta_json).unwrap(),
                Some(data.to_vec()),
            )
        } else {
            (serde_json::from_slice(&r.body).unwrap(), None)
        };
        let target = (method == "PATCH").then(|| id.unwrap().to_owned());
        let (name, parent) = match &target {
            Some(_) => (String::new(), String::new()),
            None => (
                meta["name"].as_str().unwrap().to_owned(),
                meta["parents"][0].as_str().unwrap().to_owned(),
            ),
        };
        match data {
            Some(data) => {
                let f = self.commit(target, &name, &parent, data);
                json_resp(200, Self::meta(&f))
            }
            None => {
                self.next += 1;
                let sid = format!("s{}", self.next);
                self.sessions.insert(
                    sid.clone(),
                    Session {
                        target,
                        name,
                        parent,
                        data: Vec::new(),
                    },
                );
                Resp {
                    status: 200,
                    headers: vec![(
                        "Location".into(),
                        format!("{}/upload/session/{sid}", self.base),
                    )],
                    body: vec![],
                }
            }
        }
    }

    fn commit(
        &mut self,
        target: Option<String>,
        name: &str,
        parent: &str,
        data: Vec<u8>,
    ) -> MockFile {
        let id = match target {
            Some(id) => id,
            None => self.add(name, parent, false, b""),
        };
        let f = self.find(&id).unwrap();
        f.data = data;
        f.clone()
    }

    fn session_put(&mut self, r: &Req) -> Resp {
        let sid = r.path.rsplit('/').next().unwrap().to_owned();
        if r.method == "DELETE" {
            self.sessions.remove(&sid);
            return Resp {
                status: 499,
                headers: vec![],
                body: vec![],
            };
        }
        let range = r.headers.get("content-range").cloned();
        let s = self.sessions.get_mut(&sid).unwrap();
        let total = match &range {
            None => {
                s.data.extend_from_slice(&r.body);
                Some(s.data.len())
            }
            Some(cr) => {
                let (span, total) = cr.trim_start_matches("bytes ").split_once('/').unwrap();
                if span != "*" {
                    let start: usize = span.split('-').next().unwrap().parse().unwrap();
                    assert_eq!(start, s.data.len(), "조각 시작 위치");
                    s.data.extend_from_slice(&r.body);
                }
                total.parse::<usize>().ok()
            }
        };
        match total {
            Some(t) => {
                assert_eq!(t, s.data.len());
                let s = self.sessions.remove(&sid).unwrap();
                let f = self.commit(s.target, &s.name, &s.parent, s.data);
                json_resp(200, Self::meta(&f))
            }
            None => Resp {
                status: 308,
                headers: vec![("Range".into(), format!("bytes=0-{}", s.data.len() - 1))],
                body: vec![],
            },
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn split_bytes(hay: &[u8], sep: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = hay;
    while let Some(i) = find(rest, sep) {
        out.push(rest[..i].to_vec());
        rest = &rest[i + sep.len()..];
    }
    out.push(rest.to_vec());
    out
}

async fn read_req(stream: &mut tokio::net::TcpStream) -> Option<Req> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    let head_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_owned();
    let target = first.next()?.to_owned();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let mut body = buf[head_end + 4..].to_vec();
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while body.len() < len {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let query = form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    Some(Req {
        method,
        path: path.to_owned(),
        query,
        headers,
        body,
    })
}

async fn mock() -> (GoogleDriveBackend, Arc<StdMutex<MockState>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mut st = MockState {
        base: base.clone(),
        ..Default::default()
    };
    st.files.push(MockFile {
        id: "root".into(),
        name: "My Drive".into(),
        parent: String::new(),
        folder: true,
        doc: false,
        data: vec![],
        modified: String::new(),
        trashed: false,
    });
    let state = Arc::new(StdMutex::new(st));
    let s = state.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let s = s.clone();
            tokio::spawn(async move {
                let Some(req) = read_req(&mut stream).await else {
                    return;
                };
                let resp = s.lock().unwrap().handle(req);
                let reason = if resp.status < 400 { "OK" } else { "ERR" };
                let mut out = format!(
                    "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
                    resp.status,
                    resp.body.len()
                );
                for (k, v) in &resp.headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                let _ = stream.write_all(out.as_bytes()).await;
                let _ = stream.write_all(&resp.body).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    let cfg = GoogleDriveConfig {
        client_id: "cid".into(),
        client_secret: None,
        refresh_token: SecretSource::Env {
            var: "UNUSED".into(),
        },
        shared_drive_id: None,
        root_folder_id: None,
        account: None,
    };
    let be = GoogleDriveBackend::build(
        &cfg,
        http_client(None).unwrap(),
        SecretString::from("rt"),
        Some(SecretString::from("cs")),
        Endpoints {
            api: format!("{base}/drive/v3"),
            upload: format!("{base}/upload/drive/v3"),
            token: format!("{base}/token"),
        },
    )
    .unwrap();
    be.verify().await.unwrap();
    (be, state)
}

#[tokio::test]
async fn lists_reads_and_resolves_nested_paths() {
    let (be, st) = mock().await;
    {
        let mut s = st.lock().unwrap();
        let docs = s.add("문서", "root", true, b"");
        s.add("a.txt", &docs, false, b"hello world");
        s.add("it's.txt", &docs, false, b"q");
        s.add("b.bin", &docs, false, b"0123456789");
        s.add("c.bin", "root", false, b"");
    }
    let mut names: Vec<String> = be
        .list(&p("/문서"))
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    names.sort();
    assert_eq!(names, ["a.txt", "b.bin", "it's.txt"]); // 두 쪽에 걸친 목록
    assert_eq!(be.stat(&p("/문서/it's.txt")).await.unwrap().size, 1);
    assert_eq!(
        &be.read_range(&p("/문서/a.txt"), 6, 100).await.unwrap()[..],
        b"world"
    );
    assert!(
        be.read_range(&p("/문서/a.txt"), 11, 4)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(matches!(be.stat(&p("/없음/x")).await, Err(Error::NotFound)));
    assert!(matches!(
        be.list(&p("/c.bin")).await,
        Err(Error::NotADirectory)
    ));
    assert_eq!(be.quota().await.unwrap().free, Some(750));
    assert_eq!(
        be.account_email().await.unwrap().as_deref(),
        Some("me@example.com")
    );
    assert_eq!(st.lock().unwrap().token_calls, 1);
}

#[tokio::test]
async fn refreshes_an_expired_token_once() {
    let (be, st) = mock().await;
    st.lock().unwrap().reject_next = true;
    be.list(&RemotePath::root()).await.unwrap();
    assert_eq!(st.lock().unwrap().token_calls, 2);
}

#[tokio::test]
async fn uploads_small_large_and_streamed_files() {
    let (be, st) = mock().await;
    be.mkdir(&p("/up")).await.unwrap();
    assert!(matches!(
        be.mkdir(&p("/up")).await,
        Err(Error::AlreadyExists)
    ));

    let e = be
        .write(
            &p("/up/small.txt"),
            WriteSource::Bytes(Bytes::from_static(b"abc")),
        )
        .await
        .unwrap();
    assert_eq!((e.name.as_str(), e.size), ("small.txt", 3));
    // 같은 이름은 새 파일을 만들지 않고 내용을 바꿉니다.
    be.write(
        &p("/up/small.txt"),
        WriteSource::Bytes(Bytes::from_static(b"abcd")),
    )
    .await
    .unwrap();
    assert_eq!(be.list(&p("/up")).await.unwrap().len(), 1);
    assert_eq!(be.stat(&p("/up/small.txt")).await.unwrap().size, 4);

    let big = Bytes::from(vec![7u8; 6 * MIB]);
    let e = be
        .write(&p("/up/big.bin"), WriteSource::Bytes(big))
        .await
        .unwrap();
    assert_eq!(e.size as usize, 6 * MIB);

    let mut up = be
        .begin_upload(
            &p("/up/stream.bin"),
            UploadHint {
                size: Some((9 * MIB) as u64),
                new_file: true,
            },
        )
        .await
        .unwrap();
    for i in 0..9u64 {
        up.write(i * MIB as u64, Bytes::from(vec![i as u8; MIB]))
            .await
            .unwrap();
    }
    assert!(matches!(
        up.write(0, Bytes::from_static(b"x")).await,
        Err(Error::Unsupported(_))
    ));
    let e = up.finish().await.unwrap();
    assert_eq!(e.size as usize, 9 * MIB);
    let tail = be
        .read_range(&p("/up/stream.bin"), (9 * MIB - 2) as u64, 2)
        .await
        .unwrap();
    assert_eq!(&tail[..], &[8, 8]);
    let puts = st
        .lock()
        .unwrap()
        .log
        .iter()
        .filter(|l| l.starts_with("PUT /upload/session/"))
        .count();
    assert_eq!(puts, 3, "6MiB 한 번, 스트리밍 8MiB 조각과 마지막 조각");

    // 빈 파일 스트리밍
    let up = be
        .begin_upload(&p("/up/empty"), UploadHint::default())
        .await
        .unwrap();
    assert_eq!(up.finish().await.unwrap().size, 0);
}

#[tokio::test]
async fn renames_moves_and_trashes() {
    let (be, st) = mock().await;
    be.mkdir(&p("/a")).await.unwrap();
    be.mkdir(&p("/b")).await.unwrap();
    be.write(&p("/a/x.txt"), WriteSource::Bytes(Bytes::from_static(b"1")))
        .await
        .unwrap();
    be.write(
        &p("/b/y.txt"),
        WriteSource::Bytes(Bytes::from_static(b"22")),
    )
    .await
    .unwrap();

    assert!(matches!(
        be.rename(&p("/a/x.txt"), &p("/b/y.txt"), false).await,
        Err(Error::AlreadyExists)
    ));
    be.rename(&p("/a/x.txt"), &p("/b/y.txt"), true)
        .await
        .unwrap();
    assert_eq!(be.stat(&p("/b/y.txt")).await.unwrap().size, 1);
    assert!(matches!(
        be.stat(&p("/a/x.txt")).await,
        Err(Error::NotFound)
    ));

    // 폴더 이름 변경 뒤 하위 경로도 새 이름으로 찾습니다.
    be.rename(&p("/b"), &p("/a/c"), false).await.unwrap();
    assert_eq!(be.stat(&p("/a/c/y.txt")).await.unwrap().size, 1);
    assert!(matches!(be.list(&p("/b")).await, Err(Error::NotFound)));
    assert!(matches!(
        be.rename(&p("/a"), &p("/a/c/z"), false).await,
        Err(Error::InvalidPath(_))
    ));

    assert!(matches!(
        be.delete(&p("/a/c"), EntryKind::Dir).await,
        Err(Error::DirectoryNotEmpty)
    ));
    assert!(matches!(
        be.delete(&p("/a/c/y.txt"), EntryKind::Dir).await,
        Err(Error::NotADirectory)
    ));
    be.delete(&p("/a/c/y.txt"), EntryKind::File).await.unwrap();
    be.delete(&p("/a/c"), EntryKind::Dir).await.unwrap();
    assert!(be.list(&p("/a")).await.unwrap().is_empty());
    let trashed = st
        .lock()
        .unwrap()
        .files
        .iter()
        .filter(|f| f.trashed)
        .count();
    assert_eq!(
        trashed, 3,
        "덮어쓴 파일, 지운 파일과 폴더는 휴지통에 있습니다"
    );

    let t = SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
    be.write(&p("/m"), WriteSource::Bytes(Bytes::new()))
        .await
        .unwrap();
    be.set_mtime(&p("/m"), t).await.unwrap();
    assert_eq!(be.stat(&p("/m")).await.unwrap().modified, Some(t));
}

#[tokio::test]
async fn sign_in_receives_the_loopback_redirect_and_exchanges_the_code() {
    let token = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let token_url = format!("http://{}/token", token.local_addr().unwrap());
    let seen = Arc::new(StdMutex::new(String::new()));
    let s = seen.clone();
    tokio::spawn(async move {
        let (mut stream, _) = token.accept().await.unwrap();
        let req = read_req(&mut stream).await.unwrap();
        *s.lock().unwrap() = String::from_utf8(req.body).unwrap();
        let body = r#"{"access_token":"at","expires_in":3599,"refresh_token":"1//rt"}"#;
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(body.as_bytes()).await.unwrap();
    });
    let client = OAuthClient {
        id: "cid".into(),
        secret: Some(SecretString::from("cs")),
    };
    let http = http_client(None).unwrap();
    let opened = Arc::new(StdMutex::new(String::new()));
    let o = opened.clone();
    let auth = oauth::sign_in_with(
        &http,
        &client,
        oauth::AUTH_URL,
        &token_url,
        Duration::from_secs(20),
        move |url| {
            *o.lock().unwrap() = url.to_owned();
            let q: HashMap<String, String> = Url::parse(url)
                .unwrap()
                .query_pairs()
                .into_owned()
                .collect();
            let redirect = q["redirect_uri"].clone();
            let state = q["state"].clone();
            tokio::spawn(async move {
                let http = reqwest::Client::new();
                // 파비콘과 state가 다른 요청은 무시하고 기다립니다.
                let _ = http.get(format!("{redirect}/favicon.ico")).send().await;
                let _ = http
                    .get(format!("{redirect}/?state=wrong&code=evil"))
                    .send()
                    .await;
                let page = http
                    .get(format!("{redirect}/?state={state}&code=4%2Fgood"))
                    .send()
                    .await
                    .unwrap();
                assert!(page.text().await.unwrap().contains("로그인했습니다"));
            });
        },
    )
    .await
    .unwrap();
    assert_eq!(auth.refresh_token.expose_secret(), "1//rt");
    assert_eq!(auth.expires_in, Duration::from_secs(3599));
    let body = seen.lock().unwrap().clone();
    let form: HashMap<String, String> = form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    assert_eq!(form["code"], "4/good");
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["client_secret"], "cs");
    let url = opened.lock().unwrap().clone();
    let q: HashMap<String, String> = Url::parse(&url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(
        oauth::code_challenge(&form["code_verifier"]),
        q["code_challenge"]
    );
    assert_eq!(form["redirect_uri"], q["redirect_uri"]);
}

#[tokio::test]
async fn hidden_workspace_files_are_never_overwritten_or_trashed() {
    let (be, st) = mock().await;
    be.mkdir(&p("/w")).await.unwrap();
    let doc = {
        let mut s = st.lock().unwrap();
        let w = s.files.iter().find(|f| f.name == "w").unwrap().id.clone();
        let id = s.add("보고서.docx", &w, false, b"");
        s.find(&id).unwrap().doc = true;
        id
    };
    assert!(be.list(&p("/w")).await.unwrap().is_empty());
    assert!(matches!(
        be.stat(&p("/w/보고서.docx")).await,
        Err(Error::NotFound)
    ));
    // 숨긴 문서가 든 폴더는 지우지 않습니다.
    assert!(matches!(
        be.delete(&p("/w"), EntryKind::Dir).await,
        Err(Error::DirectoryNotEmpty)
    ));

    // 같은 이름으로 저장하면 문서를 덮어쓰지 않고 새 파일을 만듭니다.
    be.write(
        &p("/w/보고서.docx"),
        WriteSource::Bytes(Bytes::from_static(b"zip")),
    )
    .await
    .unwrap();
    assert_eq!(be.stat(&p("/w/보고서.docx")).await.unwrap().size, 3);
    be.write(
        &p("/w/tmp"),
        WriteSource::Bytes(Bytes::from_static(b"zip2")),
    )
    .await
    .unwrap();
    be.rename(&p("/w/tmp"), &p("/w/보고서.docx"), true)
        .await
        .unwrap();
    assert_eq!(be.stat(&p("/w/보고서.docx")).await.unwrap().size, 4);

    let s = st.lock().unwrap();
    let d = s.files.iter().find(|f| f.id == doc).unwrap();
    assert!(!d.trashed && d.data.is_empty(), "문서는 그대로 남습니다");
}
