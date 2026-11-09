//! WebDAV(RFC 4918) 백엔드. 클라이언트 하나(연결 풀)를 모든 요청이 공유합니다.

#[cfg(test)]
mod tests;
mod xml;

use crate::proxy::Proxy;
use crate::tls;
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use expandrv_core::config::{SecretResolver, WebDavConfig};
use expandrv_core::{
    Backend, Capabilities, Entry, EntryKind, Error, Quota, RemotePath, Result, WriteSource,
};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HeaderMap, RANGE};
use reqwest::{Method, RequestBuilder, Response, StatusCode, Url};
use secrecy::{ExposeSecret, SecretString};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use xml::{DavResponse, PROPFIND_QUOTA, PROPFIND_STAT, Props};

/// 경로 구성 요소에서 그대로 둘 문자(RFC 3986 unreserved)
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 응답 헤더 대기와 본문 조각 사이 대기 상한
const IO_TIMEOUT: Duration = Duration::from_secs(60);
/// 서버 쪽에서 오래 걸릴 수 있는 MOVE, DELETE 상한
const SLOW_OP_TIMEOUT: Duration = Duration::from_secs(300);
/// 업로드 전체 상한 계산에 쓰는 최저 처리량(바이트/초)
const MIN_UPLOAD_RATE: u64 = 32 * 1024;
const UPLOAD_CHUNK: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Propfind,
    Get,
    Put,
    Mkcol,
    Move,
    Delete,
}

impl Op {
    fn name(self) -> &'static str {
        match self {
            Op::Propfind => "PROPFIND",
            Op::Get => "GET",
            Op::Put => "PUT",
            Op::Mkcol => "MKCOL",
            Op::Move => "MOVE",
            Op::Delete => "DELETE",
        }
    }

    fn method(self) -> Method {
        match self {
            Op::Get => Method::GET,
            Op::Put => Method::PUT,
            Op::Delete => Method::DELETE,
            other => Method::from_bytes(other.name().as_bytes()).expect("유효한 HTTP 메서드"),
        }
    }
}

pub struct WebDavBackend {
    client: reqwest::Client,
    /// 후행 `/`가 있고 사용자 정보, 쿼리, 조각이 없는 루트 URL
    base: Url,
    username: Option<String>,
    password: Option<SecretString>,
}

impl WebDavBackend {
    /// 설정으로 클라이언트를 만들고 루트에 PROPFIND(Depth 0)를 보내 연결과 인증을 확인합니다.
    pub async fn connect(cfg: &WebDavConfig, secrets: &dyn SecretResolver) -> Result<Self> {
        Self::connect_via(cfg, secrets, None).await
    }

    /// 프록시를 거쳐 연결합니다.
    pub async fn connect_via(
        cfg: &WebDavConfig,
        secrets: &dyn SecretResolver,
        proxy: Option<Arc<Proxy>>,
    ) -> Result<Self> {
        let be = Self::build(cfg, secrets, proxy.as_deref())?;
        be.verify().await?;
        Ok(be)
    }

    #[cfg(test)]
    fn new(cfg: &WebDavConfig, secrets: &dyn SecretResolver) -> Result<Self> {
        Self::build(cfg, secrets, None)
    }

    fn build(
        cfg: &WebDavConfig,
        secrets: &dyn SecretResolver,
        proxy: Option<&Proxy>,
    ) -> Result<Self> {
        let mut base = Url::parse(cfg.url.trim())
            .map_err(|e| Error::Config(format!("WebDAV URL이 올바르지 않습니다: {e}")))?;
        let https = match base.scheme() {
            "https" => true,
            "http" => false,
            s => {
                return Err(Error::Config(format!(
                    "WebDAV URL 스킴 {s}는 지원하지 않습니다"
                )));
            }
        };
        if !base.username().is_empty() || base.password().is_some() {
            return Err(Error::Config(
                "WebDAV URL에 자격 증명을 넣지 말고 username과 password 설정을 사용하세요".into(),
            ));
        }
        base.set_query(None);
        base.set_fragment(None);
        if !base.path().ends_with('/') {
            let p = format!("{}/", base.path());
            base.set_path(&p);
        }

        let password = cfg
            .password
            .as_ref()
            .map(|s| secrets.resolve(s))
            .transpose()?;
        if password.is_some() && cfg.username.is_none() {
            return Err(Error::Config(
                "WebDAV password가 있으면 username도 필요합니다".into(),
            ));
        }
        if !https && cfg.username.is_some() {
            tracing::warn!("WebDAV 기본 인증 정보를 평문 HTTP로 보냅니다. https 사용을 권장합니다");
        }

        crate::install_crypto_provider();
        let mut builder = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("ExpanDrv/", env!("CARGO_PKG_VERSION")))
            .tls_version_min(reqwest::tls::Version::TLS_1_2);
        if let Some(p) = proxy {
            builder = builder.proxy(p.reqwest()?);
        }
        if let Some(pin) = &cfg.pinned_cert_sha256 {
            if !https {
                return Err(Error::Config(
                    "pinned_cert_sha256은 https URL에서만 사용할 수 있습니다".into(),
                ));
            }
            let mut tls = tls::pinned_client_config(tls::parse_fingerprint(pin)?)?;
            tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            builder = builder.tls_backend_preconfigured(tls);
        }
        let client = builder.build().map_err(|e| {
            Error::Config(format!(
                "HTTP 클라이언트를 만들지 못했습니다: {}",
                error_chain(&e.without_url())
            ))
        })?;

        Ok(Self {
            client,
            base,
            username: cfg.username.clone(),
            password,
        })
    }

    async fn verify(&self) -> Result<()> {
        let not_found = || Error::Config("WebDAV 루트 URL을 찾을 수 없습니다".into());
        match self.propfind(self.base.clone(), "0", PROPFIND_STAT).await {
            Ok((_, list)) => match list.into_iter().find(|r| r.ok) {
                Some(r) if r.props.collection => Ok(()),
                Some(_) => Err(Error::Config(
                    "WebDAV 루트 URL이 컬렉션(폴더)이 아닙니다".into(),
                )),
                None => Err(not_found()),
            },
            Err(Error::PermissionDenied) => {
                Err(Error::Auth("서버가 접근을 거부했습니다(HTTP 403)".into()))
            }
            Err(Error::NotFound) => Err(not_found()),
            Err(e) => Err(e),
        }
    }

    /// 구성 요소마다 퍼센트 인코딩해 루트 URL 뒤에 붙입니다. 컬렉션은 후행 `/`를 붙입니다.
    fn url(&self, path: &RemotePath, dir: bool) -> Result<Url> {
        let mut s = self.base.as_str().to_owned();
        for (i, c) in path.components().enumerate() {
            if i > 0 {
                s.push('/');
            }
            s.extend(utf8_percent_encode(c, SEGMENT));
        }
        if dir && !path.is_root() {
            s.push('/');
        }
        Url::parse(&s).map_err(|e| Error::InvalidPath(format!("{path}: {e}")))
    }

    fn request(&self, op: Op, url: Url) -> RequestBuilder {
        let rb = self.client.request(op.method(), url);
        match &self.username {
            Some(user) => rb.basic_auth(user, self.password.as_ref().map(|p| p.expose_secret())),
            None => rb,
        }
    }

    /// 디렉터리를 후행 `/` 없이 요청해 리디렉션되면 `/`를 붙여 한 번 다시 시도합니다.
    async fn propfind(
        &self,
        mut url: Url,
        depth: &'static str,
        body: &'static str,
    ) -> Result<(Url, Vec<DavResponse>)> {
        let mut retried = false;
        loop {
            let rb = self
                .request(Op::Propfind, url.clone())
                .header("Depth", depth)
                .header(CONTENT_TYPE, "application/xml; charset=utf-8")
                .body(body);
            let mut resp = send(rb, IO_TIMEOUT).await?;
            let status = resp.status();
            if status.is_redirection() && !retried && !url.path().ends_with('/') {
                retried = true;
                let p = format!("{}/", url.path());
                url.set_path(&p);
                continue;
            }
            if status != StatusCode::MULTI_STATUS && status != StatusCode::OK {
                return Err(status_error(status, Op::Propfind));
            }
            let raw = collect(&mut resp, 0, u64::MAX).await?;
            let text = String::from_utf8_lossy(&raw);
            let list = xml::parse_multistatus(&text).map_err(|e| {
                Error::Protocol(format!("PROPFIND 응답을 해석하지 못했습니다: {e}"))
            })?;
            return Ok((url, list));
        }
    }

    /// Depth 1 목록에서 자기 자신을 빼고 하위 응답만 돌려줍니다. 대상이 파일이면 `NotADirectory`입니다.
    async fn children(&self, path: &RemotePath) -> Result<Vec<(String, DavResponse)>> {
        let (url, resps) = match self
            .propfind(self.url(path, true)?, "1", PROPFIND_STAT)
            .await
        {
            // 파일 경로에 후행 `/`를 붙이면 404를 주는 서버가 있어 종류를 다시 확인합니다.
            Err(Error::NotFound) if !path.is_root() => {
                return match self.stat(path).await {
                    Ok(e) if !e.is_dir() => Err(Error::NotADirectory),
                    Ok(_) => Err(Error::NotFound),
                    Err(e) => Err(e),
                };
            }
            r => r?,
        };
        let req = percent_decode_str(url.path()).decode_utf8_lossy();
        let (me, children) = split_self(&req, resps);
        match me {
            Some(me) if me.props.collection => Ok(children),
            Some(_) => Err(Error::NotADirectory),
            None => Err(Error::NotFound),
        }
    }
}

#[async_trait]
impl Backend for WebDavBackend {
    fn kind(&self) -> &'static str {
        "webdav"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            range_read: true,
            set_mtime: false,
            rename_dir: true,
            case_sensitive: true,
            sequential_transfer: false,
        }
    }

    async fn stat(&self, path: &RemotePath) -> Result<Entry> {
        let (_, list) = self
            .propfind(self.url(path, path.is_root())?, "0", PROPFIND_STAT)
            .await?;
        let r = list.into_iter().find(|r| r.ok).ok_or(Error::NotFound)?;
        Ok(entry_from(path.name().unwrap_or(""), r.props))
    }

    async fn list(&self, path: &RemotePath) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for (p, r) in self.children(path).await? {
            let name = xml::last_segment(&p);
            if !r.ok {
                continue;
            }
            if name.is_empty() || RemotePath::root().join(name).is_err() {
                tracing::warn!(dir = %path, "Windows에서 쓸 수 없는 이름을 건너뜁니다");
                continue;
            }
            out.push(entry_from(name, r.props));
        }
        Ok(out)
    }

    async fn read_range(&self, path: &RemotePath, offset: u64, len: u64) -> Result<Bytes> {
        if len == 0 {
            return Ok(Bytes::new());
        }
        let end = offset.saturating_add(len - 1);
        let rb = self
            .request(Op::Get, self.url(path, false)?)
            .header(RANGE, format!("bytes={offset}-{end}"));
        let mut resp = send(rb, IO_TIMEOUT).await?;
        match resp.status() {
            StatusCode::PARTIAL_CONTENT => {
                if content_range_start(resp.headers()).is_some_and(|s| s != offset) {
                    return Err(Error::Protocol(
                        "서버가 요청과 다른 범위를 돌려주었습니다".into(),
                    ));
                }
                collect(&mut resp, 0, len).await
            }
            // Range를 무시하는 서버는 전체 본문을 주므로 앞부분을 건너뜁니다.
            StatusCode::OK => collect(&mut resp, offset, len).await,
            StatusCode::RANGE_NOT_SATISFIABLE => Ok(Bytes::new()),
            s => Err(status_error(s, Op::Get)),
        }
    }

    async fn write(&self, path: &RemotePath, source: WriteSource) -> Result<Entry> {
        if path.is_root() {
            return Err(Error::IsADirectory);
        }
        let rb = self.request(Op::Put, self.url(path, false)?);
        let (rb, size) = match source {
            WriteSource::Bytes(b) => {
                let n = b.len() as u64;
                (rb.body(b), n)
            }
            WriteSource::File { path: src, .. } => {
                let file = tokio::fs::File::open(&src).await?;
                let n = file.metadata().await?.len();
                let stream =
                    tokio_util::io::ReaderStream::with_capacity(file.take(n), UPLOAD_CHUNK);
                // 청크 전송을 받지 않는 서버가 있어 길이를 명시합니다.
                let rb = rb
                    .header(CONTENT_LENGTH, n)
                    .body(reqwest::Body::wrap_stream(stream));
                (rb, n)
            }
        };
        let limit = IO_TIMEOUT + Duration::from_secs(size / MIN_UPLOAD_RATE);
        check(send(rb, limit).await?, Op::Put)?;
        self.stat(path).await
    }

    async fn mkdir(&self, path: &RemotePath) -> Result<Entry> {
        if path.is_root() {
            return Err(Error::AlreadyExists);
        }
        // 기존 컬렉션에도 201을 돌려주는 서버(rclone 등)가 있어 먼저 확인합니다.
        match self.stat(path).await {
            Ok(_) => return Err(Error::AlreadyExists),
            Err(Error::NotFound) => {}
            Err(e) => return Err(e),
        }
        let rb = self.request(Op::Mkcol, self.url(path, true)?);
        check(send(rb, IO_TIMEOUT).await?, Op::Mkcol)?;
        self.stat(path).await
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> Result<()> {
        if from == to {
            return Ok(());
        }
        if from.is_root() || to.is_root() {
            return Err(Error::PermissionDenied);
        }
        // 대소문자만 바뀌는 경우 대소문자 무시 서버에서 자기 자신을 덮어쓰지 않도록 덮어쓰기를 끕니다.
        let case_only = from.as_str().to_lowercase() == to.as_str().to_lowercase();
        let overwrite = replace && !case_only;
        if overwrite {
            // Overwrite: T는 대상 컬렉션을 통째로 지우므로 디렉터리 대상은 거부합니다.
            match self.stat(to).await {
                Ok(e) if e.is_dir() => return Err(Error::IsADirectory),
                Ok(_) | Err(Error::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        let rb = self
            .request(Op::Move, self.url(from, false)?)
            .header("Destination", self.url(to, false)?.as_str())
            .header("Overwrite", if overwrite { "T" } else { "F" });
        match check(send(rb, SLOW_OP_TIMEOUT).await?, Op::Move) {
            Ok(_) => Ok(()),
            // 원본이 없을 때 403을 주는 서버(golang.org/x/net/webdav 계열)가 있습니다.
            Err(Error::PermissionDenied) => match self.stat(from).await {
                Err(Error::NotFound) => Err(Error::NotFound),
                _ => Err(Error::PermissionDenied),
            },
            Err(e) => Err(e),
        }
    }

    async fn delete(&self, path: &RemotePath, kind: EntryKind) -> Result<()> {
        if path.is_root() {
            return Err(Error::PermissionDenied);
        }
        // DELETE는 컬렉션을 재귀 삭제하므로 종류와 비어 있음을 먼저 확인합니다.
        let dir = kind == EntryKind::Dir;
        if dir {
            if !self.children(path).await?.is_empty() {
                return Err(Error::DirectoryNotEmpty);
            }
        } else if self.stat(path).await?.is_dir() {
            return Err(Error::IsADirectory);
        }
        let rb = self.request(Op::Delete, self.url(path, dir)?);
        check(send(rb, SLOW_OP_TIMEOUT).await?, Op::Delete)?;
        Ok(())
    }

    async fn quota(&self) -> Result<Quota> {
        let list = match self.propfind(self.base.clone(), "0", PROPFIND_QUOTA).await {
            Ok((_, list)) => list,
            // 할당량 속성을 모르는 서버는 오류 상태를 주기도 합니다.
            Err(Error::Protocol(e)) => {
                tracing::debug!("WebDAV 할당량 조회 실패: {e}");
                return Ok(Quota::default());
            }
            Err(e) => return Err(e),
        };
        let Some(p) = list.into_iter().find(|r| r.ok).map(|r| r.props) else {
            return Ok(Quota::default());
        };
        let total = match (p.quota_used, p.quota_available) {
            (Some(u), Some(f)) => u.checked_add(f),
            _ => None,
        };
        Ok(Quota {
            total,
            used: p.quota_used,
            free: p.quota_available,
        })
    }
}

fn entry_from(name: &str, p: Props) -> Entry {
    Entry {
        name: name.to_owned(),
        kind: if p.collection {
            EntryKind::Dir
        } else {
            EntryKind::File
        },
        size: if p.collection {
            0
        } else {
            p.content_length.unwrap_or(0)
        },
        modified: p.last_modified,
        created: p.creation_date,
        etag: p.etag,
        readonly: false,
    }
}

/// Depth 1 응답을 자기 자신과 하위 항목(디코딩한 경로, 응답)으로 나눕니다.
/// href 접두가 요청 URL과 다른 서버(프록시 재작성, 유니코드 정규화 차이)는 가장 짧은 경로를 자기 자신으로 봅니다.
fn split_self(
    request_path: &str,
    resps: Vec<DavResponse>,
) -> (Option<DavResponse>, Vec<(String, DavResponse)>) {
    let req = xml::normalize(request_path);
    let mut items: Vec<(String, DavResponse)> = resps
        .into_iter()
        .map(|r| (xml::normalize(&xml::href_path(&r.href, &req)), r))
        .collect();
    let exact = items.iter().position(|(p, _)| *p == req);
    let me_idx = exact.or_else(|| {
        items
            .iter()
            .enumerate()
            .min_by_key(|(_, (p, _))| p.len())
            .map(|(i, _)| i)
    });
    let me = me_idx.map(|i| items.remove(i).1);
    if exact.is_some() {
        // Depth 1을 무시하고 더 깊이 돌려주는 서버에 대비해 직계 하위만 남깁니다.
        let prefix = if req == "/" {
            "/".to_owned()
        } else {
            format!("{req}/")
        };
        items.retain(|(p, _)| {
            p.strip_prefix(&prefix)
                .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
        });
    }
    (me, items)
}

async fn send(rb: RequestBuilder, limit: Duration) -> Result<Response> {
    match tokio::time::timeout(limit, rb.send()).await {
        Ok(r) => r.map_err(map_reqwest),
        Err(_) => Err(Error::Timeout),
    }
}

fn check(resp: Response, op: Op) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        Ok(resp)
    } else {
        Err(status_error(status, op))
    }
}

/// 본문에서 `skip` 바이트를 건너뛰고 최대 `len` 바이트를 모읍니다. 다 모으면 나머지는 받지 않습니다.
async fn collect(resp: &mut Response, mut skip: u64, len: u64) -> Result<Bytes> {
    let hint = resp.content_length().unwrap_or(64 << 10);
    let mut buf = BytesMut::with_capacity(len.min(hint).min(16 << 20) as usize);
    while (buf.len() as u64) < len {
        let chunk = match tokio::time::timeout(IO_TIMEOUT, resp.chunk()).await {
            Ok(r) => r.map_err(map_reqwest)?,
            Err(_) => return Err(Error::Timeout),
        };
        let Some(mut chunk) = chunk else {
            break;
        };
        if skip > 0 {
            if chunk.len() as u64 <= skip {
                skip -= chunk.len() as u64;
                continue;
            }
            chunk = chunk.slice(skip as usize..);
            skip = 0;
        }
        let take = (len - buf.len() as u64).min(chunk.len() as u64) as usize;
        buf.extend_from_slice(&chunk[..take]);
    }
    Ok(buf.freeze())
}

/// `Content-Range: bytes 100-199/1000`의 시작 오프셋
fn content_range_start(headers: &HeaderMap) -> Option<u64> {
    let v = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    let range = v.trim().strip_prefix("bytes")?.trim_start();
    range.split('-').next()?.trim().parse().ok()
}

fn status_error(status: StatusCode, op: Op) -> Error {
    match status.as_u16() {
        404 | 410 => Error::NotFound,
        401 => Error::Auth("서버가 자격 증명을 거부했습니다(HTTP 401)".into()),
        403 => Error::PermissionDenied,
        405 if op == Op::Mkcol => Error::AlreadyExists,
        // 부모 컬렉션이 없을 때 409를 돌려줍니다.
        409 => Error::NotFound,
        412 if op == Op::Move => Error::AlreadyExists,
        423 => Error::PermissionDenied,
        507 => Error::StorageFull,
        408 | 504 => Error::Timeout,
        c @ (502 | 503) => {
            Error::Network(format!("서버를 일시적으로 사용할 수 없습니다(HTTP {c})"))
        }
        c => Error::Protocol(format!("{} 실패: HTTP {c}", op.name())),
    }
}

/// reqwest 오류에서 URL을 지우고 원인 사슬을 이어 붙입니다.
fn map_reqwest(e: reqwest::Error) -> Error {
    let e = e.without_url();
    if e.is_timeout() {
        return Error::Timeout;
    }
    let msg = error_chain(&e);
    if e.is_builder() {
        Error::Config(msg)
    } else if e.is_decode() {
        Error::Protocol(msg)
    } else {
        Error::Network(msg)
    }
}

fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(c) = src {
        let m = c.to_string();
        if !s.contains(&m) {
            s.push_str(": ");
            s.push_str(&m);
        }
        src = c.source();
    }
    s
}
