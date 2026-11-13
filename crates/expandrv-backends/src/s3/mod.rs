//! S3 호환 백엔드. rusty-s3로 SigV4 사전 서명 URL을 만들고 reqwest로 전송합니다.
//!
//! 디렉터리는 `/` 구분자 키 접두사로 표현하고, 빈 디렉터리는 0바이트 표식 객체(`dir/`)로 유지합니다.

#[cfg(test)]
mod tests;
mod xml;

use crate::proxy::Proxy;
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use expandrv_core::config::{S3Config, SecretResolver};
use expandrv_core::{
    Backend, Capabilities, Entry, EntryKind, Error, RemotePath, Result, UploadHint, UploadSession,
    WriteSource,
};
use futures::{StreamExt, TryStreamExt};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, ETAG, HeaderMap, LAST_MODIFIED, RANGE};
use reqwest::{Body, RequestBuilder, Response, StatusCode, Url};
use rusty_s3::{Bucket, Credentials, Method, UrlStyle};
use secrecy::{ExposeSecret, SecretString};
use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const MIB: u64 = 1 << 20;
/// 사전 서명 URL 유효 시간. 요청 시작 시점에만 검사됩니다.
const SIGN_EXPIRES_SECS: u64 = 3600;
/// 이 크기 이하 파일은 단일 PUT으로 스트리밍합니다.
const SINGLE_PUT_MAX: u64 = 16 * MIB;
const PART_SIZE: u64 = 8 * MIB;
const MAX_PARTS: u64 = 10_000;
const PART_CONCURRENCY: usize = 4;
/// CopyObject 단일 요청 상한. 넘으면 UploadPartCopy를 씁니다.
const COPY_MAX: u64 = 5 << 30;
const COPY_PART_SIZE: u64 = 512 * MIB;

/// x-amz-copy-source 키 인코딩. 비예약 문자와 `/`만 그대로 둡니다.
const COPY_SOURCE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~')
    .remove(b'/');

#[derive(Clone)]
pub struct S3Backend {
    http: reqwest::Client,
    bucket: Bucket,
    creds: Credentials,
    /// 정규화된 루트 접두사. 비어 있거나 `a/b/` 형태입니다.
    prefix: String,
    copy_threshold: u64,
    copy_part_size: u64,
}

impl S3Backend {
    /// 설정으로 백엔드를 만들고 버킷 접근을 확인합니다.
    pub async fn connect(cfg: &S3Config, secrets: &dyn SecretResolver) -> Result<Self> {
        Self::connect_via(cfg, secrets, None).await
    }

    /// 프록시를 거쳐 연결합니다.
    pub async fn connect_via(
        cfg: &S3Config,
        secrets: &dyn SecretResolver,
        proxy: Option<Arc<Proxy>>,
    ) -> Result<Self> {
        let secret = secrets.resolve(&cfg.secret_access_key)?;
        let be = Self::build(cfg, &secret, proxy.as_deref())?;
        be.verify().await?;
        Ok(be)
    }

    #[cfg(test)]
    fn new(cfg: &S3Config, secret: &SecretString) -> Result<Self> {
        Self::build(cfg, secret, None)
    }

    fn build(cfg: &S3Config, secret: &SecretString, proxy: Option<&Proxy>) -> Result<Self> {
        if cfg.bucket.trim().is_empty() {
            return Err(Error::Config("S3 버킷 이름이 비어 있습니다".into()));
        }
        let endpoint = parse_endpoint(&cfg.endpoint)?;
        let style = if cfg.path_style {
            UrlStyle::Path
        } else {
            UrlStyle::VirtualHost
        };
        let region = match cfg.region.trim() {
            "" => "us-east-1".to_owned(),
            r => r.to_owned(),
        };
        let bucket = Bucket::new(endpoint, style, cfg.bucket.trim().to_owned(), region)
            .map_err(|e| Error::Config(format!("S3 엔드포인트가 올바르지 않습니다: {e:?}")))?;
        crate::install_crypto_provider();
        let mut http = reqwest::Client::builder();
        if let Some(p) = proxy {
            http = http.proxy(p.reqwest()?);
        }
        let http = http
            .user_agent(concat!("ExpanDrv/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(120))
            // 서명 URL이 다른 호스트로 전달되지 않도록 리디렉션을 따르지 않습니다.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| Error::Config(format!("HTTP 클라이언트 생성 실패: {e}")))?;
        Ok(Self {
            http,
            bucket,
            creds: Credentials::new(cfg.access_key_id.clone(), secret.expose_secret()),
            prefix: normalize_prefix(&cfg.prefix)?,
            copy_threshold: COPY_MAX,
            copy_part_size: COPY_PART_SIZE,
        })
    }

    async fn verify(&self) -> Result<()> {
        match self.list_page(&self.prefix, true, Some(1), None).await {
            Ok(_) => Ok(()),
            Err(Error::PermissionDenied) => Err(Error::Auth("버킷 접근이 거부되었습니다".into())),
            Err(e) => Err(e),
        }
    }

    fn key(&self, path: &RemotePath) -> String {
        format!("{}{}", self.prefix, path.relative())
    }

    /// 디렉터리 하위 키 접두사. 루트는 루트 접두사 자체입니다.
    fn dir_prefix(&self, path: &RemotePath) -> String {
        if path.is_root() {
            self.prefix.clone()
        } else {
            format!("{}{}/", self.prefix, path.relative())
        }
    }

    /// 서명된 요청을 만듭니다. `signed` 헤더는 서명에 포함되고 요청에도 그대로 붙습니다.
    fn request(
        &self,
        method: Method,
        key: Option<&str>,
        query: &[(&str, &str)],
        signed: &[(&str, &str)],
    ) -> Result<RequestBuilder> {
        let url = match key {
            Some(k) => self
                .bucket
                .object_url(k)
                .map_err(|e| Error::InvalidPath(e.to_string()))?,
            None => self.bucket.base_url().clone(),
        };
        let mut q = query.to_vec();
        q.sort_unstable();
        let mut h = signed.to_vec();
        h.sort_unstable();
        let url = rusty_s3::signing::sign(
            &jiff::Timestamp::now(),
            method,
            url,
            self.creds.key(),
            self.creds.secret(),
            self.creds.token(),
            self.bucket.region(),
            SIGN_EXPIRES_SECS,
            q.iter().copied(),
            h.iter().copied(),
        );
        let m = match method {
            Method::Head => reqwest::Method::HEAD,
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Delete => reqwest::Method::DELETE,
        };
        let mut rb = self.http.request(m, url);
        for (k, v) in h {
            rb = rb.header(k, v);
        }
        Ok(rb)
    }

    /// 전송만 하고 상태 코드는 호출자가 판단합니다.
    async fn send(&self, rb: RequestBuilder) -> Result<Response> {
        rb.send().await.map_err(net_err)
    }

    /// 2xx가 아니면 오류로 바꿉니다.
    async fn exec(&self, rb: RequestBuilder) -> Result<Response> {
        let resp = self.send(rb).await?;
        if resp.status().is_success() {
            Ok(resp)
        } else {
            Err(error_from(resp).await)
        }
    }

    /// 2xx 본문을 읽고, 200이어도 본문이 `<Error>`면 오류로 처리합니다.
    async fn exec_xml(&self, rb: RequestBuilder) -> Result<String> {
        let resp = self.exec(rb).await?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(net_err)?;
        if xml::root_name(&text).as_deref() == Some("Error") {
            let (code, msg) = xml::parse_error(&text);
            return Err(xml::map_status(status, code.as_deref(), msg.as_deref()));
        }
        Ok(text)
    }

    async fn list_page(
        &self,
        prefix: &str,
        delimiter: bool,
        max_keys: Option<u32>,
        token: Option<&str>,
    ) -> Result<xml::ListPage> {
        let mut q = vec![("list-type", "2"), ("prefix", prefix)];
        if delimiter {
            q.push(("delimiter", "/"));
        }
        let mk = max_keys.map(|m| m.to_string());
        if let Some(m) = &mk {
            q.push(("max-keys", m));
        }
        if let Some(t) = token {
            q.push(("continuation-token", t));
        }
        let text = self
            .exec_xml(self.request(Method::Get, None, &q, &[])?)
            .await?;
        xml::parse_list(&text)
    }

    /// 객체가 있으면 응답 헤더, 없으면 `None`
    async fn head(&self, key: &str) -> Result<Option<HeaderMap>> {
        let resp = self
            .send(self.request(Method::Head, Some(key), &[], &[])?)
            .await?;
        match resp.status() {
            s if s.is_success() => Ok(Some(resp.headers().clone())),
            StatusCode::NOT_FOUND => Ok(None),
            _ => Err(error_from(resp).await),
        }
    }

    /// 표식 객체나 하위 키가 하나라도 있으면 디렉터리로 봅니다.
    async fn dir_exists(&self, path: &RemotePath) -> Result<bool> {
        if path.is_root() {
            return Ok(true);
        }
        let page = self
            .list_page(&self.dir_prefix(path), false, Some(1), None)
            .await?;
        Ok(!page.contents.is_empty() || !page.common_prefixes.is_empty())
    }

    async fn delete_key(&self, key: &str) -> Result<()> {
        self.exec(self.request(Method::Delete, Some(key), &[], &[])?)
            .await?;
        Ok(())
    }

    async fn put_bytes(&self, key: &str, data: Bytes) -> Result<()> {
        let rb = self
            .request(Method::Put, Some(key), &[], &[])?
            .header(CONTENT_LENGTH, data.len())
            .body(data);
        self.exec(rb).await?;
        Ok(())
    }

    async fn put_file(&self, key: &str, src: &Path) -> Result<()> {
        let file = tokio::fs::File::open(src).await?;
        let size = file.metadata().await?.len();
        if size > SINGLE_PUT_MAX {
            return self.put_multipart(key, src, size).await;
        }
        if size == 0 {
            return self.put_bytes(key, Bytes::new()).await;
        }
        let body = Body::wrap_stream(tokio_util::io::ReaderStream::new(file.take(size)));
        // S3는 chunked 전송을 받지 않으므로 길이를 명시합니다.
        let rb = self
            .request(Method::Put, Some(key), &[], &[])?
            .header(CONTENT_LENGTH, size)
            .body(body);
        self.exec(rb).await?;
        Ok(())
    }

    async fn create_multipart(&self, key: &str) -> Result<String> {
        let rb = self.request(Method::Post, Some(key), &[("uploads", "")], &[])?;
        xml::parse_upload_id(&self.exec_xml(rb).await?)
    }

    /// 파트 결과로 업로드를 완료하고, 실패하면 업로드를 중단해 남은 파트를 정리합니다.
    async fn finish_multipart(
        &self,
        key: &str,
        upload_id: &str,
        parts: Result<Vec<(u32, String)>>,
    ) -> Result<()> {
        let res = match parts {
            Ok(mut parts) => {
                parts.sort_unstable_by_key(|p| p.0);
                let rb = self
                    .request(Method::Post, Some(key), &[("uploadId", upload_id)], &[])?
                    .header(CONTENT_TYPE, "application/xml")
                    .body(xml::complete_body(&parts));
                self.exec_xml(rb).await.map(|_| ())
            }
            Err(e) => Err(e),
        };
        if res.is_err() {
            self.abort_multipart(key, upload_id).await;
        }
        res
    }

    async fn abort_multipart(&self, key: &str, upload_id: &str) {
        let abort = match self.request(Method::Delete, Some(key), &[("uploadId", upload_id)], &[]) {
            Ok(rb) => self.exec(rb).await.map(|_| ()),
            Err(e) => Err(e),
        };
        if let Err(e) = abort {
            tracing::warn!(error = %e, "S3 멀티파트 업로드 중단 실패");
        }
    }

    async fn put_multipart(&self, key: &str, src: &Path, size: u64) -> Result<()> {
        let part = part_size(size, PART_SIZE);
        let upload_id = self.create_multipart(key).await?;
        let parts = futures::stream::iter(part_ranges(size, part))
            .map(|(n, off, len)| self.upload_part(key, &upload_id, n, src, off, len))
            .buffer_unordered(PART_CONCURRENCY)
            .try_collect()
            .await;
        self.finish_multipart(key, &upload_id, parts).await
    }

    async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        num: u32,
        src: &Path,
        offset: u64,
        len: u64,
    ) -> Result<(u32, String)> {
        let mut f = tokio::fs::File::open(src).await?;
        f.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut buf = vec![0u8; len as usize];
        f.read_exact(&mut buf).await?;
        self.upload_part_bytes(key, upload_id, num, Bytes::from(buf))
            .await
    }

    async fn upload_part_bytes(
        &self,
        key: &str,
        upload_id: &str,
        num: u32,
        data: Bytes,
    ) -> Result<(u32, String)> {
        let n = num.to_string();
        let rb = self
            .request(
                Method::Put,
                Some(key),
                &[("partNumber", &n), ("uploadId", upload_id)],
                &[],
            )?
            .header(CONTENT_LENGTH, data.len())
            .body(data);
        let resp = self.exec(rb).await?;
        let etag = resp
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Error::Protocol("업로드 파트 응답에 ETag가 없습니다".into()))?;
        Ok((num, etag.to_owned()))
    }

    fn copy_source(&self, key: &str) -> String {
        format!(
            "/{}/{}",
            self.bucket.name(),
            utf8_percent_encode(key, COPY_SOURCE)
        )
    }

    async fn copy_object(&self, src: &str, dst: &str, size: u64) -> Result<()> {
        let source = self.copy_source(src);
        if size <= self.copy_threshold {
            let rb = self
                .request(
                    Method::Put,
                    Some(dst),
                    &[],
                    &[("x-amz-copy-source", &source)],
                )?
                .header(CONTENT_LENGTH, 0);
            self.exec_xml(rb).await?;
            return Ok(());
        }
        let part = part_size(size, self.copy_part_size);
        let upload_id = self.create_multipart(dst).await?;
        let parts = futures::stream::iter(part_ranges(size, part))
            .map(|(n, off, len)| self.copy_part(dst, &upload_id, n, &source, off, len))
            .buffer_unordered(PART_CONCURRENCY)
            .try_collect()
            .await;
        self.finish_multipart(dst, &upload_id, parts).await
    }

    async fn copy_part(
        &self,
        key: &str,
        upload_id: &str,
        num: u32,
        source: &str,
        offset: u64,
        len: u64,
    ) -> Result<(u32, String)> {
        let n = num.to_string();
        let range = format!("bytes={}-{}", offset, offset + len - 1);
        let rb = self
            .request(
                Method::Put,
                Some(key),
                &[("partNumber", &n), ("uploadId", upload_id)],
                &[
                    ("x-amz-copy-source", source),
                    ("x-amz-copy-source-range", &range),
                ],
            )?
            .header(CONTENT_LENGTH, 0);
        let etag = xml::parse_copy_etag(&self.exec_xml(rb).await?)?;
        Ok((num, etag))
    }
}

#[async_trait]
impl Backend for S3Backend {
    fn kind(&self) -> &'static str {
        "s3"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            range_read: true,
            set_mtime: false,
            rename_dir: false,
            case_sensitive: true,
            sequential_transfer: false,
            streaming_upload: true,
            random_write: false,
        }
    }

    async fn stat(&self, path: &RemotePath) -> Result<Entry> {
        let Some(name) = path.name() else {
            return Ok(Entry::dir(""));
        };
        if let Some(h) = self.head(&self.key(path)).await? {
            return Ok(file_entry(name, &h));
        }
        if self.dir_exists(path).await? {
            Ok(Entry::dir(name))
        } else {
            Err(Error::NotFound)
        }
    }

    async fn list(&self, path: &RemotePath) -> Result<Vec<Entry>> {
        let prefix = self.dir_prefix(path);
        let mut out = Vec::new();
        let mut seen_any = false;
        let mut token: Option<String> = None;
        loop {
            let page = self
                .list_page(&prefix, true, None, token.as_deref())
                .await?;
            seen_any |= !page.contents.is_empty() || !page.common_prefixes.is_empty();
            page_entries(&prefix, &page, &mut out);
            match page.next_continuation_token {
                Some(t) if page.is_truncated && !t.is_empty() => token = Some(t),
                _ => break,
            }
        }
        if !seen_any && !path.is_root() {
            return Err(if self.head(&self.key(path)).await?.is_some() {
                Error::NotADirectory
            } else {
                Error::NotFound
            });
        }
        Ok(dedupe_entries(out))
    }

    async fn read_range(&self, path: &RemotePath, offset: u64, len: u64) -> Result<Bytes> {
        if len == 0 {
            return Ok(Bytes::new());
        }
        let end = offset.saturating_add(len - 1);
        let rb = self
            .request(Method::Get, Some(&self.key(path)), &[], &[])?
            .header(RANGE, format!("bytes={offset}-{end}"));
        let resp = self.send(rb).await?;
        match resp.status() {
            StatusCode::PARTIAL_CONTENT => resp.bytes().await.map_err(net_err),
            // Range를 무시한 서버는 전체 본문에서 잘라냅니다.
            StatusCode::OK => {
                let all = resp.bytes().await.map_err(net_err)?;
                let start = offset.min(all.len() as u64) as usize;
                let stop = offset.saturating_add(len).min(all.len() as u64) as usize;
                Ok(all.slice(start..stop))
            }
            StatusCode::RANGE_NOT_SATISFIABLE => Ok(Bytes::new()),
            _ => {
                let err = error_from(resp).await;
                if matches!(err, Error::NotFound) && self.dir_exists(path).await? {
                    Err(Error::IsADirectory)
                } else {
                    Err(err)
                }
            }
        }
    }

    async fn write(&self, path: &RemotePath, source: WriteSource) -> Result<Entry> {
        if path.is_root() {
            return Err(Error::IsADirectory);
        }
        let key = self.key(path);
        match source {
            WriteSource::Bytes(b) => self.put_bytes(&key, b).await?,
            WriteSource::File { path: src, .. } => self.put_file(&key, &src).await?,
        }
        self.stat(path).await
    }

    async fn begin_upload(
        &self,
        path: &RemotePath,
        hint: UploadHint,
    ) -> Result<Box<dyn UploadSession>> {
        if path.is_root() {
            return Err(Error::IsADirectory);
        }
        let part = part_size(hint.size.unwrap_or(0), PART_SIZE);
        Ok(Box::new(S3Upload {
            backend: self.clone(),
            key: self.key(path),
            path: path.clone(),
            part,
            buf: BytesMut::new(),
            upload_id: None,
            next_part: 1,
            sent: 0,
            inflight: VecDeque::new(),
            parts: Vec::new(),
        }))
    }

    async fn mkdir(&self, path: &RemotePath) -> Result<Entry> {
        let Some(name) = path.name() else {
            return Err(Error::AlreadyExists);
        };
        if self.head(&self.key(path)).await?.is_some() || self.dir_exists(path).await? {
            return Err(Error::AlreadyExists);
        }
        self.put_bytes(&self.dir_prefix(path), Bytes::new()).await?;
        Ok(Entry::dir(name))
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> Result<()> {
        if from.is_root() || to.is_root() {
            return Err(Error::InvalidPath("루트는 이름을 바꿀 수 없습니다".into()));
        }
        let src = self.stat(from).await?;
        if src.is_dir() {
            return Err(Error::Unsupported("S3 디렉터리 이름 변경"));
        }
        let (src_key, dst_key) = (self.key(from), self.key(to));
        if src_key == dst_key {
            return Ok(());
        }
        match self.stat(to).await {
            Ok(_) if !replace => return Err(Error::AlreadyExists),
            Ok(e) if e.is_dir() => return Err(Error::IsADirectory),
            Ok(_) | Err(Error::NotFound) => {}
            Err(e) => return Err(e),
        }
        self.copy_object(&src_key, &dst_key, src.size).await?;
        self.delete_key(&src_key).await
    }

    async fn delete(&self, path: &RemotePath, kind: EntryKind) -> Result<()> {
        if path.is_root() {
            return Err(Error::InvalidPath("루트는 삭제할 수 없습니다".into()));
        }
        let key = self.key(path);
        match kind {
            EntryKind::File => {
                // S3 DELETE는 없는 키에도 성공하므로 먼저 존재를 확인합니다.
                if self.head(&key).await?.is_none() {
                    return Err(Error::NotFound);
                }
                self.delete_key(&key).await
            }
            EntryKind::Dir => {
                let prefix = self.dir_prefix(path);
                // 표식 `dir/`는 하위 키보다 앞에 정렬되므로 두 개만 보면 충분합니다.
                let page = self.list_page(&prefix, false, Some(2), None).await?;
                if page.contents.is_empty() {
                    return Err(if self.head(&key).await?.is_some() {
                        Error::NotADirectory
                    } else {
                        Error::NotFound
                    });
                }
                if page.contents.iter().any(|o| o.key != prefix) {
                    return Err(Error::DirectoryNotEmpty);
                }
                self.delete_key(&prefix).await
            }
        }
    }
}

fn parse_endpoint(raw: &str) -> Result<Url> {
    let mut url = Url::parse(raw.trim())
        .map_err(|e| Error::Config(format!("S3 엔드포인트를 해석할 수 없습니다: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(Error::Config(
            "S3 엔드포인트는 http(s)://호스트 형태여야 합니다".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::Config(
            "S3 엔드포인트에 사용자 정보를 넣을 수 없습니다".into(),
        ));
    }
    url.set_query(None);
    url.set_fragment(None);
    if !url.path().ends_with('/') {
        let p = format!("{}/", url.path());
        url.set_path(&p);
    }
    Ok(url)
}

/// `\`와 `/`를 모두 구분자로 받아 빈 문자열 또는 `a/b/` 형태로 만듭니다.
fn normalize_prefix(raw: &str) -> Result<String> {
    let parts: Vec<&str> = raw.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    if parts.iter().any(|p| *p == "." || *p == "..") {
        return Err(Error::Config(format!(
            "S3 접두사 {raw:?}가 올바르지 않습니다"
        )));
    }
    Ok(if parts.is_empty() {
        String::new()
    } else {
        format!("{}/", parts.join("/"))
    })
}

// ---------- 스트리밍 업로드 ----------

/// 파트 크기만큼 모아 멀티파트로 올리는 세션. 파트 하나를 넘지 않는 파일은 확정 시 단일 PUT으로 올립니다.
struct S3Upload {
    backend: S3Backend,
    key: String,
    path: RemotePath,
    part: u64,
    buf: BytesMut,
    upload_id: Option<String>,
    next_part: u32,
    sent: u64,
    inflight: VecDeque<tokio::task::JoinHandle<Result<(u32, String)>>>,
    parts: Vec<(u32, String)>,
}

impl S3Upload {
    async fn reap(&mut self, keep: usize) -> Result<()> {
        while self.inflight.len() > keep {
            let task = self.inflight.pop_front().expect("inflight");
            let part = task
                .await
                .map_err(|e| Error::Io(std::io::Error::other(e)))??;
            self.parts.push(part);
        }
        Ok(())
    }

    /// 모아 둔 파트를 올립니다. 첫 파트에서 멀티파트 업로드를 시작합니다.
    async fn push_part(&mut self, data: Bytes) -> Result<()> {
        if self.upload_id.is_none() {
            self.upload_id = Some(self.backend.create_multipart(&self.key).await?);
        }
        self.reap(PART_CONCURRENCY - 1).await?;
        let (be, key, id) = (
            self.backend.clone(),
            self.key.clone(),
            self.upload_id.clone().expect("upload id"),
        );
        let num = self.next_part;
        self.next_part += 1;
        self.inflight.push_back(tokio::spawn(async move {
            be.upload_part_bytes(&key, &id, num, data).await
        }));
        Ok(())
    }

    async fn discard(&mut self) {
        while let Some(task) = self.inflight.pop_front() {
            let _ = task.await;
        }
        if let Some(id) = self.upload_id.take() {
            self.backend.abort_multipart(&self.key, &id).await;
        }
    }
}

#[async_trait]
impl UploadSession for S3Upload {
    async fn write(&mut self, offset: u64, data: Bytes) -> Result<()> {
        if offset != self.sent {
            return Err(Error::Unsupported("S3 순서 밖 쓰기"));
        }
        self.sent += data.len() as u64;
        self.buf.extend_from_slice(&data);
        while self.buf.len() as u64 >= self.part {
            let chunk = self.buf.split_to(self.part as usize).freeze();
            self.push_part(chunk).await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        self.reap(0).await
    }

    async fn finish(mut self: Box<Self>) -> Result<Entry> {
        let res = async {
            match self.upload_id.clone() {
                None => {
                    let data = std::mem::take(&mut self.buf).freeze();
                    self.backend.put_bytes(&self.key, data).await?;
                }
                Some(id) => {
                    if !self.buf.is_empty() {
                        let data = std::mem::take(&mut self.buf).freeze();
                        self.push_part(data).await?;
                    }
                    self.reap(0).await?;
                    let parts = std::mem::take(&mut self.parts);
                    self.upload_id = None;
                    self.backend
                        .finish_multipart(&self.key, &id, Ok(parts))
                        .await?;
                }
            }
            self.backend.stat(&self.path).await
        }
        .await;
        if res.is_err() {
            self.discard().await;
        }
        res
    }

    async fn abort(mut self: Box<Self>) {
        self.discard().await;
    }
}

/// 파트 크기. 기본 크기에서 시작해 파트 수가 상한을 넘으면 MiB 단위로 키웁니다.
fn part_size(total: u64, base: u64) -> u64 {
    if total.div_ceil(base) <= MAX_PARTS {
        base
    } else {
        total.div_ceil(MAX_PARTS).div_ceil(MIB) * MIB
    }
}

/// (파트 번호, 오프셋, 길이) 목록
fn part_ranges(total: u64, part: u64) -> Vec<(u32, u64, u64)> {
    (0..total.div_ceil(part))
        .map(|i| {
            let off = i * part;
            (i as u32 + 1, off, part.min(total - off))
        })
        .collect()
}

fn net_err(e: reqwest::Error) -> Error {
    // 오류 문자열에 서명 URL이 들어가지 않도록 제거합니다.
    let e = e.without_url();
    if e.is_timeout() {
        return Error::Timeout;
    }
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    Error::Network(msg)
}

async fn error_from(resp: Response) -> Error {
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let (code, msg) = xml::parse_error(&text);
    xml::map_status(status, code.as_deref(), msg.as_deref())
}

fn trim_etag(s: &str) -> String {
    s.trim().trim_matches('"').to_owned()
}

fn parse_http_date(s: &str) -> Option<SystemTime> {
    jiff::fmt::rfc2822::DateTimeParser::new()
        .parse_timestamp(s.trim())
        .ok()
        .map(SystemTime::from)
}

fn parse_iso_date(s: &str) -> Option<SystemTime> {
    s.trim()
        .parse::<jiff::Timestamp>()
        .ok()
        .map(SystemTime::from)
}

fn file_entry(name: &str, h: &HeaderMap) -> Entry {
    let get = |k| {
        h.get(k)
            .and_then(|v: &reqwest::header::HeaderValue| v.to_str().ok())
    };
    let size = get(CONTENT_LENGTH)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    Entry {
        modified: get(LAST_MODIFIED).and_then(parse_http_date),
        etag: get(ETAG).map(trim_etag),
        ..Entry::file(name, size)
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && RemotePath::root().join(name).is_ok()
}

/// 목록 한 페이지를 항목으로 바꿉니다. 디렉터리 표식 자신과 드라이브에서 쓸 수 없는 이름은 건너뜁니다.
fn page_entries(prefix: &str, page: &xml::ListPage, out: &mut Vec<Entry>) {
    for o in &page.contents {
        let Some(name) = o.key.strip_prefix(prefix) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        if !valid_name(name) {
            tracing::debug!(key = %o.key, "드라이브에서 쓸 수 없는 S3 키를 건너뜁니다");
            continue;
        }
        out.push(Entry {
            modified: o.last_modified.as_deref().and_then(parse_iso_date),
            etag: o.etag.as_deref().map(trim_etag),
            ..Entry::file(name, o.size)
        });
    }
    for p in &page.common_prefixes {
        let Some(name) = p
            .prefix
            .strip_prefix(prefix)
            .and_then(|n| n.strip_suffix('/'))
        else {
            continue;
        };
        if !valid_name(name) {
            tracing::debug!(prefix = %p.prefix, "드라이브에서 쓸 수 없는 S3 접두사를 건너뜁니다");
            continue;
        }
        out.push(Entry::dir(name));
    }
}

/// 같은 이름의 객체와 접두사가 함께 있으면 `stat`과 같게 파일을 남깁니다.
fn dedupe_entries(mut entries: Vec<Entry>) -> Vec<Entry> {
    let files: HashSet<String> = entries
        .iter()
        .filter(|e| !e.is_dir())
        .map(|e| e.name.clone())
        .collect();
    entries.retain(|e| !e.is_dir() || !files.contains(&e.name));
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries.dedup_by(|a, b| a.name == b.name);
    entries
}
