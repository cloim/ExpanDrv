//! Google Drive 백엔드(Drive API v3). 경로를 파일 ID로 해석해 REST로 접근합니다.
//!
//! - 경로 해석: 부모 폴더 ID 아래에서 이름으로 찾고 폴더 ID는 메모리에 보관합니다. 같은 이름이
//!   여러 개면 가장 최근에 수정한 항목 하나만 보입니다(목록과 조회가 같은 기준을 씁니다).
//! - Google 문서, 스프레드시트 같은 Workspace 파일과 바로가기는 내려받을 내용이 없어 목록에서 뺍니다.
//! - 삭제는 Drive 휴지통으로 옮깁니다. 네트워크 드라이브는 탐색기 휴지통을 거치지 않으므로
//!   실수로 지운 파일을 Drive 웹에서 되살릴 수 있습니다.
//! - 업로드는 재개 가능 업로드 세션을 씁니다. 스트리밍 업로드는 8MiB 단위로 보내고 마지막에 확정합니다.

pub mod oauth;
#[cfg(test)]
mod tests;

use crate::proxy::Proxy;
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use expandrv_core::config::{GoogleDriveConfig, SecretResolver};
use expandrv_core::{
    Backend, Capabilities, Entry, EntryKind, Error, Quota, RemotePath, Result, UploadHint,
    UploadSession, WriteSource,
};
use oauth::OAuthClient;
use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, LOCATION, RANGE};
use reqwest::{Body, RequestBuilder, Response, StatusCode, Url};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

const MIB: usize = 1 << 20;
const API_BASE: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD_BASE: &str = "https://www.googleapis.com/upload/drive/v3";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const FILE_FIELDS: &str =
    "id,name,mimeType,size,modifiedTime,createdTime,md5Checksum,version,capabilities(canEdit)";
/// 이 크기 이하는 메타데이터와 내용을 한 요청(multipart)으로 올립니다.
const MULTIPART_MAX: usize = 5 * MIB;
/// 재개 가능 업로드 조각 크기. 256KiB의 배수여야 합니다.
const CHUNK: usize = 8 * MIB;
const PAGE_SIZE: &str = "1000";
const MAX_RETRIES: u32 = 4;
/// 만료 직전 토큰을 쓰지 않도록 미리 갱신합니다.
const TOKEN_MARGIN: Duration = Duration::from_secs(60);

/// API 주소. 테스트에서 로컬 서버로 바꿉니다.
#[derive(Debug, Clone)]
pub(crate) struct Endpoints {
    pub api: String,
    pub upload: String,
    pub token: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            api: API_BASE.into(),
            upload: UPLOAD_BASE.into(),
            token: oauth::TOKEN_URL.into(),
        }
    }
}

#[derive(Clone)]
pub struct GoogleDriveBackend {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    ep: Endpoints,
    client: OAuthClient,
    refresh_token: SecretString,
    token: tokio::sync::Mutex<Option<(SecretString, Instant)>>,
    shared_drive: Option<String>,
    root_id: String,
    /// 폴더 경로 → 폴더 ID. 루트는 넣지 않습니다.
    dirs: Mutex<HashMap<RemotePath, String>>,
}

/// Drive 파일 리소스에서 쓰는 필드
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DriveFile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub mime_type: String,
    /// int64를 문자열로 보냅니다. 폴더와 Workspace 파일에는 없습니다.
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub modified_time: Option<String>,
    #[serde(default)]
    pub created_time: Option<String>,
    #[serde(default)]
    pub md5_checksum: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub capabilities: Option<FileCapabilities>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileCapabilities {
    #[serde(default)]
    pub can_edit: Option<bool>,
}

impl DriveFile {
    fn is_folder(&self) -> bool {
        self.mime_type == FOLDER_MIME
    }

    /// 파일 시스템에 보일 항목. Workspace 파일, 바로가기, Windows에서 쓸 수 없는 이름은 `None`입니다.
    pub(crate) fn entry(&self) -> Option<Entry> {
        if !self.is_folder() && self.mime_type.starts_with("application/vnd.google-apps.") {
            return None;
        }
        if self.name.contains(['/', '\\']) || RemotePath::root().join(&self.name).is_err() {
            return None;
        }
        let mut e = if self.is_folder() {
            Entry::dir(self.name.clone())
        } else {
            Entry::file(
                self.name.clone(),
                self.size
                    .as_deref()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
            )
        };
        e.modified = self.modified_time.as_deref().and_then(parse_time);
        e.created = self.created_time.as_deref().and_then(parse_time);
        if !self.is_folder() {
            e.etag = self
                .md5_checksum
                .clone()
                .or_else(|| self.version.as_ref().map(|v| format!("v{v}")));
        }
        e.readonly = self
            .capabilities
            .as_ref()
            .and_then(|c| c.can_edit)
            .is_some_and(|edit| !edit);
        Some(e)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileList {
    #[serde(default)]
    next_page_token: Option<String>,
    #[serde(default)]
    files: Vec<DriveFile>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct About {
    #[serde(default)]
    user: Option<AboutUser>,
    #[serde(default)]
    storage_quota: Option<StorageQuota>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AboutUser {
    #[serde(default)]
    email_address: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StorageQuota {
    #[serde(default)]
    limit: Option<String>,
    #[serde(default)]
    usage: Option<String>,
}

#[derive(Deserialize)]
struct ApiErrorBody {
    error: ApiError,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
    message: String,
    #[serde(default)]
    errors: Vec<ApiErrorItem>,
}

#[derive(Deserialize)]
struct ApiErrorItem {
    #[serde(default)]
    reason: String,
}

fn parse_time(s: &str) -> Option<SystemTime> {
    s.trim()
        .parse::<jiff::Timestamp>()
        .ok()
        .map(SystemTime::from)
}

fn format_time(t: SystemTime) -> Result<String> {
    let ts = jiff::Timestamp::try_from(t)
        .map_err(|e| Error::InvalidPath(format!("수정 시각을 표현할 수 없습니다: {e}")))?;
    Ok(ts.round(jiff::Unit::Millisecond).unwrap_or(ts).to_string())
}

/// Drive 검색 문자열 안의 따옴표 값
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Drive ID에는 영문, 숫자, `-`, `_`만 씁니다. URL 경로에 넣기 전에 확인합니다.
pub(crate) fn check_id(what: &str, id: &str) -> Result<String> {
    let id = id.trim();
    if id.is_empty()
        || id.len() > 256
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(Error::Config(format!("{what} {id:?}가 올바르지 않습니다")));
    }
    Ok(id.to_owned())
}

pub(crate) fn net_err(e: reqwest::Error) -> Error {
    // 업로드 세션 주소가 오류 문자열에 남지 않도록 지웁니다.
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

/// 재시도할 만한 응답인지와 사용자에게 보일 오류
pub(crate) fn api_error(status: u16, body: &str) -> (bool, Error) {
    let parsed = serde_json::from_str::<ApiErrorBody>(body).ok();
    let message = parsed
        .as_ref()
        .map(|b| b.error.message.clone())
        .unwrap_or_default();
    let reasons: Vec<&str> = parsed
        .as_ref()
        .map(|b| b.error.errors.iter().map(|e| e.reason.as_str()).collect())
        .unwrap_or_default();
    let has = |r: &str| reasons.contains(&r);
    let detail = |what: &str| {
        if message.is_empty() {
            what.to_owned()
        } else {
            format!("{what}: {message}")
        }
    };
    match status {
        401 => (false, Error::Auth(detail("Google 인증이 거부되었습니다"))),
        403 if has("rateLimitExceeded") || has("userRateLimitExceeded") => (
            true,
            Error::Network(detail("Google Drive 요청 한도를 넘었습니다")),
        ),
        403 if has("storageQuotaExceeded") => (false, Error::StorageFull),
        403 if has("accessNotConfigured") || has("SERVICE_DISABLED") => (
            false,
            Error::Config(detail(
                "Google Cloud 프로젝트에서 Google Drive API를 사용 설정하세요",
            )),
        ),
        403 if has("insufficientPermissions") || has("ACCESS_TOKEN_SCOPE_INSUFFICIENT") => (
            false,
            Error::Auth(detail("Drive 접근 범위가 부족합니다. 다시 로그인하세요")),
        ),
        403 => (false, Error::PermissionDenied),
        404 => (false, Error::NotFound),
        409 => (false, Error::AlreadyExists),
        429 => (
            true,
            Error::Network(detail("Google Drive 요청이 너무 많습니다")),
        ),
        500..=599 => (
            true,
            Error::Network(detail(&format!("Google Drive 서버 오류(HTTP {status})"))),
        ),
        _ => (
            false,
            Error::Protocol(detail(&format!("Google Drive 응답 HTTP {status}"))),
        ),
    }
}

/// Drive API 요청에 공통으로 붙이는 공유 드라이브 지원 표시
fn with_drive_params(mut url: Url, params: &[(&str, &str)]) -> Url {
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("supportsAllDrives", "true");
        for (k, v) in params {
            q.append_pair(k, v);
        }
    }
    url
}

fn json_body(value: &serde_json::Value) -> Bytes {
    Bytes::from(serde_json::to_vec(value).unwrap_or_default())
}

impl GoogleDriveBackend {
    /// 설정으로 백엔드를 만들고 루트 폴더 접근을 확인합니다.
    pub async fn connect(cfg: &GoogleDriveConfig, secrets: &dyn SecretResolver) -> Result<Self> {
        Self::connect_via(cfg, secrets, None).await
    }

    pub async fn connect_via(
        cfg: &GoogleDriveConfig,
        secrets: &dyn SecretResolver,
        proxy: Option<Arc<Proxy>>,
    ) -> Result<Self> {
        let refresh_token = secrets.resolve(&cfg.refresh_token)?;
        let secret = cfg
            .client_secret
            .as_ref()
            .map(|s| secrets.resolve(s))
            .transpose()?;
        let http = http_client(proxy.as_deref())?;
        let be = Self::build(cfg, http, refresh_token, secret, Endpoints::default())?;
        be.verify().await?;
        Ok(be)
    }

    pub(crate) fn build(
        cfg: &GoogleDriveConfig,
        http: reqwest::Client,
        refresh_token: SecretString,
        client_secret: Option<SecretString>,
        ep: Endpoints,
    ) -> Result<Self> {
        let client_id = cfg.client_id.trim();
        if client_id.is_empty() || client_id.contains(char::is_whitespace) {
            return Err(Error::Config(
                "OAuth 클라이언트 ID가 올바르지 않습니다".into(),
            ));
        }
        let shared_drive = cfg
            .shared_drive_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| check_id("공유 드라이브 ID", s))
            .transpose()?;
        let root_folder = cfg
            .root_folder_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| check_id("루트 폴더 ID", s))
            .transpose()?;
        let root_id = root_folder
            .or_else(|| shared_drive.clone())
            .unwrap_or_else(|| "root".to_owned());
        Ok(Self {
            inner: Arc::new(Inner {
                http,
                ep,
                client: OAuthClient {
                    id: client_id.to_owned(),
                    secret: client_secret,
                },
                refresh_token,
                token: tokio::sync::Mutex::new(None),
                shared_drive,
                root_id,
                dirs: Mutex::new(HashMap::new()),
            }),
        })
    }

    async fn verify(&self) -> Result<()> {
        let root = self.inner.root_id.clone();
        match self.get_file(&root).await {
            Ok(f) if f.is_folder() => Ok(()),
            Ok(_) => Err(Error::Config("루트 폴더 ID가 폴더가 아닙니다".into())),
            Err(Error::NotFound) => Err(Error::Config(
                "루트 폴더 또는 공유 드라이브를 찾을 수 없습니다. ID와 접근 권한을 확인하세요"
                    .into(),
            )),
            Err(e) => Err(e),
        }
    }

    /// 로그인한 계정의 이메일 주소(표시용)
    pub async fn account_email(&self) -> Result<Option<String>> {
        let url = self.api_url("about", &[("fields", "user(emailAddress)")])?;
        let about: About = self.json(|| self.inner.http.get(url.clone())).await?;
        Ok(about.user.and_then(|u| u.email_address))
    }

    fn api_url(&self, path: &str, params: &[(&str, &str)]) -> Result<Url> {
        let url = Url::parse(&format!("{}/{path}", self.inner.ep.api))
            .map_err(|e| Error::Config(format!("API 주소 오류: {e}")))?;
        Ok(with_drive_params(url, params))
    }

    fn upload_url(&self, path: &str, params: &[(&str, &str)]) -> Result<Url> {
        let url = Url::parse(&format!("{}/{path}", self.inner.ep.upload))
            .map_err(|e| Error::Config(format!("업로드 주소 오류: {e}")))?;
        Ok(with_drive_params(url, params))
    }

    /// 유효한 접근 토큰. 만료가 가까우면 갱신 토큰으로 새로 받습니다.
    async fn access_token(&self) -> Result<SecretString> {
        let mut slot = self.inner.token.lock().await;
        if let Some((t, until)) = slot.as_ref()
            && Instant::now() + TOKEN_MARGIN < *until
        {
            return Ok(t.clone());
        }
        let fresh = oauth::refresh(
            &self.inner.http,
            &self.inner.ep.token,
            &self.inner.client,
            &self.inner.refresh_token,
        )
        .await?;
        *slot = Some((fresh.token.clone(), Instant::now() + fresh.expires_in));
        Ok(fresh.token)
    }

    /// 서버가 거부한 토큰을 버립니다. 그사이 다른 작업이 새로 받은 토큰은 유지합니다.
    async fn drop_token(&self, used: &SecretString) {
        let mut slot = self.inner.token.lock().await;
        if slot
            .as_ref()
            .is_some_and(|(t, _)| t.expose_secret() == used.expose_secret())
        {
            *slot = None;
        }
    }

    /// 요청을 보내고 2xx 또는 `accept` 상태면 응답을 돌려줍니다. 토큰 만료는 한 번 갱신하고,
    /// 요청 한도와 서버 오류는 지수 백오프로 다시 보냅니다. `build`는 매 시도마다 요청을 새로 만듭니다.
    async fn exec(
        &self,
        build: impl Fn() -> RequestBuilder,
        accept: &[StatusCode],
    ) -> Result<Response> {
        let mut refreshed = false;
        let mut attempt = 0u32;
        loop {
            let token = self.access_token().await?;
            let sent = build().bearer_auth(token.expose_secret()).send().await;
            let resp = match sent {
                Ok(r) => r,
                // 연결 전 실패만 다시 보냅니다. 응답 대기 중 시간 초과는 서버가 처리했을 수 있어
                // 생성 요청이 중복되지 않도록 그대로 돌려줍니다.
                Err(e) if attempt < MAX_RETRIES && e.is_connect() => {
                    attempt += 1;
                    backoff(attempt).await;
                    continue;
                }
                Err(e) => return Err(net_err(e)),
            };
            let status = resp.status();
            if status.is_success() || accept.contains(&status) {
                return Ok(resp);
            }
            if status == StatusCode::UNAUTHORIZED && !refreshed {
                self.drop_token(&token).await;
                refreshed = true;
                continue;
            }
            let body = resp.text().await.unwrap_or_default();
            let (retry, err) = api_error(status.as_u16(), &body);
            if retry && attempt < MAX_RETRIES {
                attempt += 1;
                backoff(attempt).await;
                continue;
            }
            return Err(err);
        }
    }

    /// 한 번만 보낼 수 있는 요청(스트림 본문). 토큰은 미리 갱신해 둡니다.
    async fn exec_once(&self, rb: RequestBuilder, accept: &[StatusCode]) -> Result<Response> {
        let token = self.access_token().await?;
        let resp = rb
            .bearer_auth(token.expose_secret())
            .send()
            .await
            .map_err(net_err)?;
        let status = resp.status();
        if status.is_success() || accept.contains(&status) {
            return Ok(resp);
        }
        if status == StatusCode::UNAUTHORIZED {
            self.drop_token(&token).await;
        }
        let body = resp.text().await.unwrap_or_default();
        Err(api_error(status.as_u16(), &body).1)
    }

    async fn json<T: serde::de::DeserializeOwned>(
        &self,
        build: impl Fn() -> RequestBuilder,
    ) -> Result<T> {
        let resp = self.exec(build, &[]).await?;
        parse_json(resp).await
    }

    async fn get_file(&self, id: &str) -> Result<DriveFile> {
        let url = self.api_url(&format!("files/{id}"), &[("fields", FILE_FIELDS)])?;
        self.json(|| self.inner.http.get(url.clone())).await
    }

    /// `files.list` 한 쪽. 공유 드라이브면 그 드라이브만 검색합니다.
    async fn list_page(
        &self,
        q: &str,
        page_size: &str,
        page_token: Option<&str>,
    ) -> Result<FileList> {
        let fields = format!("nextPageToken,files({FILE_FIELDS})");
        let mut params = vec![
            ("q", q),
            ("fields", fields.as_str()),
            ("pageSize", page_size),
            ("orderBy", "modifiedTime desc"),
            ("includeItemsFromAllDrives", "true"),
        ];
        if let Some(d) = &self.inner.shared_drive {
            params.push(("corpora", "drive"));
            params.push(("driveId", d));
        }
        if let Some(t) = page_token {
            params.push(("pageToken", t));
        }
        let url = self.api_url("files", &params)?;
        self.json(|| self.inner.http.get(url.clone())).await
    }

    /// 부모 폴더 아래 `name` 항목. 같은 이름이 여럿이면 보이는 항목 중 가장 최근에 수정한 항목이고,
    /// 보이는 항목이 없으면 숨긴 항목(Workspace 파일 등)입니다. 목록의 [`visible_entries`]와 같은 기준입니다.
    async fn lookup(&self, parent_id: &str, name: &str) -> Result<Option<DriveFile>> {
        let q = format!(
            "{} in parents and name = {} and trashed = false",
            quote(parent_id),
            quote(name)
        );
        let page = self.list_page(&q, "10", None).await?;
        // 서버 검색은 대소문자 처리 방식이 문서화되어 있지 않아 정확히 같은 이름만 받습니다.
        let mut same: Vec<DriveFile> = page.files.into_iter().filter(|f| f.name == name).collect();
        if same.is_empty() {
            return Ok(None);
        }
        let pick = same.iter().position(|f| f.entry().is_some()).unwrap_or(0);
        Ok(Some(same.swap_remove(pick)))
    }

    fn cached_dir(&self, path: &RemotePath) -> Option<String> {
        if path.is_root() {
            return Some(self.inner.root_id.clone());
        }
        self.inner.dirs.lock().unwrap().get(path).cloned()
    }

    fn remember_dir(&self, path: &RemotePath, id: &str) {
        if !path.is_root() {
            self.inner
                .dirs
                .lock()
                .unwrap()
                .insert(path.clone(), id.to_owned());
        }
    }

    /// `path`와 그 하위 경로의 폴더 ID를 잊습니다.
    fn forget(&self, path: &RemotePath) {
        self.inner
            .dirs
            .lock()
            .unwrap()
            .retain(|p, _| !p.starts_with(path));
    }

    /// 폴더 경로의 ID. 캐시에 없으면 루트에서 한 단계씩 찾습니다.
    async fn dir_id(&self, path: &RemotePath) -> Result<String> {
        if let Some(id) = self.cached_dir(path) {
            return Ok(id);
        }
        let mut cur = RemotePath::root();
        let mut id = self.inner.root_id.clone();
        for name in path.components() {
            cur = cur.join(name)?;
            if let Some(known) = self.cached_dir(&cur) {
                id = known;
                continue;
            }
            let f = self.lookup(&id, name).await?.ok_or(Error::NotFound)?;
            if !f.is_folder() {
                return Err(Error::NotADirectory);
            }
            self.remember_dir(&cur, &f.id);
            id = f.id;
        }
        Ok(id)
    }

    /// 경로의 Drive 항목과 부모 폴더 ID. 루트는 `None`입니다.
    async fn node(&self, path: &RemotePath) -> Result<Option<(DriveFile, String)>> {
        let (Some(parent), Some(name)) = (path.parent(), path.name()) else {
            return Ok(None);
        };
        let pid = match self.dir_id(&parent).await {
            Ok(id) => id,
            Err(Error::NotADirectory) => return Err(Error::NotFound),
            Err(e) => return Err(e),
        };
        let found = match self.lookup(&pid, name).await {
            // 캐시한 부모 ID가 다른 곳에서 지워졌으면 다시 찾습니다.
            Err(Error::NotFound) => {
                self.forget(&parent);
                let pid = self.dir_id(&parent).await?;
                return Ok(self.lookup(&pid, name).await?.map(|f| (f, pid)));
            }
            r => r?,
        };
        Ok(found.map(|f| (f, pid)))
    }

    /// 보이는 항목만 돌려줍니다. Workspace 파일 등 숨긴 항목은 없는 것으로 봅니다.
    async fn visible(&self, path: &RemotePath) -> Result<(DriveFile, String, Entry)> {
        let (f, pid) = self.node(path).await?.ok_or(Error::NotFound)?;
        let entry = f.entry().ok_or(Error::NotFound)?;
        if f.is_folder() {
            self.remember_dir(path, &f.id);
        }
        Ok((f, pid, entry))
    }

    async fn trash(&self, id: &str) -> Result<()> {
        let url = self.api_url(&format!("files/{id}"), &[("fields", "id")])?;
        let body = json_body(&serde_json::json!({ "trashed": true }));
        self.exec(
            || {
                self.inner
                    .http
                    .patch(url.clone())
                    .header(CONTENT_TYPE, "application/json")
                    .body(body.clone())
            },
            &[],
        )
        .await?;
        Ok(())
    }

    /// 새 파일이면 `parents`와 이름을, 기존 파일이면 빈 메타데이터를 보냅니다.
    fn upload_target(
        &self,
        existing: Option<&DriveFile>,
        parent_id: &str,
        name: &str,
        params: &[(&str, &str)],
    ) -> Result<(reqwest::Method, Url, serde_json::Value)> {
        Ok(match existing {
            Some(f) => (
                reqwest::Method::PATCH,
                self.upload_url(&format!("files/{}", f.id), params)?,
                serde_json::json!({}),
            ),
            None => (
                reqwest::Method::POST,
                self.upload_url("files", params)?,
                serde_json::json!({ "name": name, "parents": [parent_id] }),
            ),
        })
    }

    /// 대상 파일을 찾고 폴더면 거부합니다.
    async fn upload_parent(
        &self,
        path: &RemotePath,
        known_new: bool,
    ) -> Result<(String, String, Option<DriveFile>)> {
        let (Some(parent), Some(name)) = (path.parent(), path.name()) else {
            return Err(Error::IsADirectory);
        };
        let pid = self.dir_id(&parent).await?;
        // 숨긴 Workspace 파일은 내용으로 덮어쓰지 않고 같은 이름의 새 파일을 만듭니다.
        let existing = if known_new {
            None
        } else {
            self.lookup(&pid, name)
                .await?
                .filter(|f| f.entry().is_some())
        };
        if existing.as_ref().is_some_and(DriveFile::is_folder) {
            return Err(Error::IsADirectory);
        }
        Ok((pid, name.to_owned(), existing))
    }

    async fn upload_multipart(
        &self,
        existing: Option<&DriveFile>,
        pid: &str,
        name: &str,
        data: Bytes,
    ) -> Result<DriveFile> {
        let params = [("uploadType", "multipart"), ("fields", FILE_FIELDS)];
        let (method, url, meta) = self.upload_target(existing, pid, name, &params)?;
        let boundary = format!("expandrv-{}", oauth::random_token(18)?);
        let mut body = BytesMut::with_capacity(data.len() + 512);
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n\
                 --{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(&data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let body = body.freeze();
        let content_type = format!("multipart/related; boundary={boundary}");
        let resp = self
            .exec(
                || {
                    self.inner
                        .http
                        .request(method.clone(), url.clone())
                        .header(CONTENT_TYPE, &content_type)
                        .header(CONTENT_LENGTH, body.len())
                        .body(body.clone())
                },
                &[],
            )
            .await?;
        parse_json(resp).await
    }

    /// 재개 가능 업로드 세션을 열고 세션 주소를 돌려줍니다.
    async fn start_session(
        &self,
        existing: Option<&DriveFile>,
        pid: &str,
        name: &str,
        size: Option<u64>,
    ) -> Result<Url> {
        let params = [("uploadType", "resumable"), ("fields", FILE_FIELDS)];
        let (method, url, meta) = self.upload_target(existing, pid, name, &params)?;
        let meta = json_body(&meta);
        let resp = self
            .exec(
                || {
                    let mut rb = self
                        .inner
                        .http
                        .request(method.clone(), url.clone())
                        .header(CONTENT_TYPE, "application/json; charset=UTF-8")
                        .header("X-Upload-Content-Type", "application/octet-stream")
                        .body(meta.clone());
                    if let Some(n) = size {
                        rb = rb.header("X-Upload-Content-Length", n);
                    }
                    rb
                },
                &[],
            )
            .await?;
        let location = resp
            .headers()
            .get(LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| Error::Protocol("업로드 세션 주소가 없습니다".into()))?;
        let session = Url::parse(location)
            .map_err(|_| Error::Protocol("업로드 세션 주소가 올바르지 않습니다".into()))?;
        // 세션 주소가 API와 다른 호스트로 가면 토큰을 보내지 않도록 거부합니다.
        let upload = Url::parse(&self.inner.ep.upload)
            .map_err(|e| Error::Config(format!("업로드 주소 오류: {e}")))?;
        if session.scheme() != upload.scheme() || session.host_str() != upload.host_str() {
            return Err(Error::Protocol(
                "업로드 세션 주소의 호스트가 예상과 다릅니다".into(),
            ));
        }
        Ok(session)
    }

    fn remember_file(&self, path: &RemotePath, f: &DriveFile) -> Result<Entry> {
        let mut e = f.entry().ok_or_else(|| {
            Error::Protocol("업로드한 파일을 파일 시스템 항목으로 볼 수 없습니다".into())
        })?;
        if let Some(name) = path.name() {
            e.name = name.to_owned();
        }
        Ok(e)
    }
}

fn http_client(proxy: Option<&Proxy>) -> Result<reqwest::Client> {
    crate::install_crypto_provider();
    let mut b = reqwest::Client::builder();
    if let Some(p) = proxy {
        b = b.proxy(p.reqwest()?);
    }
    b.user_agent(concat!("ExpanDrv/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(120))
        // 재개 업로드의 308 응답을 리디렉션으로 따르지 않고, 토큰을 다른 호스트로 보내지 않습니다.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| Error::Config(format!("HTTP 클라이언트 생성 실패: {e}")))
}

/// 로그인에 쓸 HTTP 클라이언트. 드라이브의 프록시 설정을 따릅니다.
pub fn sign_in_client(
    proxy: Option<&expandrv_core::config::ProxyConfig>,
    secrets: &dyn SecretResolver,
) -> Result<reqwest::Client> {
    let proxy = proxy.map(|p| Proxy::from_config(p, secrets)).transpose()?;
    http_client(proxy.as_ref())
}

/// 로그인 직후 계정 이메일을 확인합니다. 실패하면 `None`입니다.
pub async fn account_for(
    http: &reqwest::Client,
    client: &OAuthClient,
    auth: &oauth::Authorization,
) -> Option<String> {
    let cfg = GoogleDriveConfig {
        client_id: client.id.clone(),
        client_secret: None,
        refresh_token: expandrv_core::config::SecretSource::Env { var: String::new() },
        shared_drive_id: None,
        root_folder_id: None,
        account: None,
    };
    let be = GoogleDriveBackend::build(
        &cfg,
        http.clone(),
        auth.refresh_token.clone(),
        client.secret.clone(),
        Endpoints::default(),
    )
    .ok()?;
    *be.inner.token.lock().await =
        Some((auth.access_token.clone(), Instant::now() + auth.expires_in));
    be.account_email().await.ok().flatten()
}

async fn parse_json<T: serde::de::DeserializeOwned>(resp: Response) -> Result<T> {
    let bytes = resp.bytes().await.map_err(net_err)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| Error::Protocol(format!("Google Drive 응답을 해석하지 못했습니다: {e}")))
}

async fn backoff(attempt: u32) {
    let ms = 500u64 << attempt.min(5);
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// 보이는 항목 중 같은 이름은 첫 번째(가장 최근 수정)만 남깁니다.
pub(crate) fn visible_entries(files: Vec<DriveFile>) -> Vec<(DriveFile, Entry)> {
    let mut seen = HashSet::new();
    files
        .into_iter()
        .filter_map(|f| f.entry().map(|e| (f, e)))
        .filter(|(f, _)| seen.insert(f.name.clone()))
        .collect()
}

#[async_trait]
impl Backend for GoogleDriveBackend {
    fn kind(&self) -> &'static str {
        "gdrive"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            range_read: true,
            set_mtime: true,
            rename_dir: true,
            case_sensitive: true,
            sequential_transfer: false,
            streaming_upload: true,
            random_write: false,
        }
    }

    fn normalize_mtime(&self, mtime: SystemTime) -> SystemTime {
        // Drive는 밀리초까지 저장합니다.
        jiff::Timestamp::try_from(mtime)
            .ok()
            .and_then(|t| t.round(jiff::Unit::Millisecond).ok())
            .map(SystemTime::from)
            .unwrap_or(mtime)
    }

    async fn stat(&self, path: &RemotePath) -> Result<Entry> {
        if path.is_root() {
            return Ok(Entry::dir(""));
        }
        Ok(self.visible(path).await?.2)
    }

    async fn list(&self, path: &RemotePath) -> Result<Vec<Entry>> {
        let id = match self.dir_id(path).await {
            Ok(id) => id,
            Err(Error::NotFound) if !path.is_root() => {
                // 다른 곳에서 옮겨진 폴더일 수 있어 한 번 더 찾습니다.
                self.forget(path);
                self.dir_id(path).await?
            }
            Err(e) => return Err(e),
        };
        let q = format!("{} in parents and trashed = false", quote(&id));
        let mut files = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let page = self.list_page(&q, PAGE_SIZE, token.as_deref()).await?;
            files.extend(page.files);
            match page.next_page_token {
                Some(t) if !t.is_empty() => token = Some(t),
                _ => break,
            }
        }
        let visible = visible_entries(files);
        {
            let mut dirs = self.inner.dirs.lock().unwrap();
            dirs.retain(|p, _| p.parent().as_ref() != Some(path));
            for (f, e) in &visible {
                if e.is_dir()
                    && let Ok(child) = path.join(&e.name)
                {
                    dirs.insert(child, f.id.clone());
                }
            }
        }
        Ok(visible.into_iter().map(|(_, e)| e).collect())
    }

    async fn read_range(&self, path: &RemotePath, offset: u64, len: u64) -> Result<Bytes> {
        if len == 0 {
            return Ok(Bytes::new());
        }
        let (f, _, e) = self.visible(path).await?;
        if e.is_dir() {
            return Err(Error::IsADirectory);
        }
        if offset >= e.size {
            return Ok(Bytes::new());
        }
        let end = offset.saturating_add(len - 1).min(e.size - 1);
        let url = self.api_url(&format!("files/{}", f.id), &[("alt", "media")])?;
        let resp = self
            .exec(
                || {
                    self.inner
                        .http
                        .get(url.clone())
                        .header(RANGE, format!("bytes={offset}-{end}"))
                },
                &[StatusCode::RANGE_NOT_SATISFIABLE],
            )
            .await?;
        match resp.status() {
            StatusCode::PARTIAL_CONTENT => resp.bytes().await.map_err(net_err),
            StatusCode::RANGE_NOT_SATISFIABLE => Ok(Bytes::new()),
            _ => {
                let all = resp.bytes().await.map_err(net_err)?;
                let start = offset.min(all.len() as u64) as usize;
                let stop = offset.saturating_add(len).min(all.len() as u64) as usize;
                Ok(all.slice(start..stop))
            }
        }
    }

    async fn write(&self, path: &RemotePath, source: WriteSource) -> Result<Entry> {
        let (pid, name, existing) = self.upload_parent(path, false).await?;
        let file = match source {
            WriteSource::Bytes(b) if b.len() <= MULTIPART_MAX => {
                self.upload_multipart(existing.as_ref(), &pid, &name, b)
                    .await?
            }
            source => {
                let size = source.len();
                let session = self
                    .start_session(existing.as_ref(), &pid, &name, Some(size))
                    .await?;
                let body = match source {
                    WriteSource::Bytes(b) => Body::from(b),
                    WriteSource::File { path: src, .. } => {
                        use tokio::io::AsyncReadExt;
                        let file = tokio::fs::File::open(&src).await?;
                        Body::wrap_stream(tokio_util::io::ReaderStream::new(file.take(size)))
                    }
                };
                let rb = self
                    .inner
                    .http
                    .put(session)
                    .header(CONTENT_LENGTH, size)
                    .body(body);
                parse_json(self.exec_once(rb, &[]).await?).await?
            }
        };
        self.remember_file(path, &file)
    }

    async fn begin_upload(
        &self,
        path: &RemotePath,
        hint: UploadHint,
    ) -> Result<Box<dyn UploadSession>> {
        let (pid, name, existing) = self.upload_parent(path, hint.new_file).await?;
        let session = self
            .start_session(existing.as_ref(), &pid, &name, hint.size)
            .await?;
        Ok(Box::new(DriveUpload {
            backend: self.clone(),
            path: path.clone(),
            session,
            buf: BytesMut::new(),
            sent: 0,
        }))
    }

    async fn mkdir(&self, path: &RemotePath) -> Result<Entry> {
        let (Some(parent), Some(name)) = (path.parent(), path.name()) else {
            return Err(Error::AlreadyExists);
        };
        let pid = self.dir_id(&parent).await?;
        if self.lookup(&pid, name).await?.is_some() {
            return Err(Error::AlreadyExists);
        }
        let url = self.api_url("files", &[("fields", FILE_FIELDS)])?;
        let body = json_body(&serde_json::json!({
            "name": name,
            "mimeType": FOLDER_MIME,
            "parents": [pid],
        }));
        let f: DriveFile = self
            .json(|| {
                self.inner
                    .http
                    .post(url.clone())
                    .header(CONTENT_TYPE, "application/json")
                    .body(body.clone())
            })
            .await?;
        self.remember_dir(path, &f.id);
        f.entry()
            .ok_or_else(|| Error::Protocol("만든 폴더를 확인하지 못했습니다".into()))
    }

    async fn rename(&self, from: &RemotePath, to: &RemotePath, replace: bool) -> Result<()> {
        if from.is_root() || to.is_root() {
            return Err(Error::InvalidPath("루트는 이름을 바꿀 수 없습니다".into()));
        }
        if from == to {
            return Ok(());
        }
        if to.starts_with(from) {
            return Err(Error::InvalidPath(
                "폴더를 자기 하위 폴더로 옮길 수 없습니다".into(),
            ));
        }
        let (src, src_parent, _) = self.visible(from).await?;
        let (Some(to_parent), Some(to_name)) = (to.parent(), to.name()) else {
            return Err(Error::InvalidPath("대상 경로가 올바르지 않습니다".into()));
        };
        let dst_parent = self.dir_id(&to_parent).await?;
        // 숨긴 Workspace 파일은 대상으로 보지 않아 휴지통으로 옮기지 않습니다.
        if let Some(existing) = self.lookup(&dst_parent, to_name).await?
            && existing.id != src.id
            && existing.entry().is_some()
        {
            if !replace {
                return Err(Error::AlreadyExists);
            }
            if existing.is_folder() {
                return Err(Error::IsADirectory);
            }
            self.trash(&existing.id).await?;
        }
        let mut params = vec![("fields", "id")];
        if dst_parent != src_parent {
            params.push(("addParents", dst_parent.as_str()));
            params.push(("removeParents", src_parent.as_str()));
        }
        let url = self.api_url(&format!("files/{}", src.id), &params)?;
        let body = json_body(&serde_json::json!({ "name": to_name }));
        self.exec(
            || {
                self.inner
                    .http
                    .patch(url.clone())
                    .header(CONTENT_TYPE, "application/json")
                    .body(body.clone())
            },
            &[],
        )
        .await?;
        self.forget(from);
        self.forget(to);
        if src.is_folder() {
            self.remember_dir(to, &src.id);
        }
        Ok(())
    }

    async fn delete(&self, path: &RemotePath, kind: EntryKind) -> Result<()> {
        if path.is_root() {
            return Err(Error::InvalidPath("루트는 삭제할 수 없습니다".into()));
        }
        let (f, _, _) = self.visible(path).await?;
        match (kind, f.is_folder()) {
            (EntryKind::File, true) => return Err(Error::IsADirectory),
            (EntryKind::Dir, false) => return Err(Error::NotADirectory),
            (EntryKind::Dir, true) => {
                // 목록에서 숨긴 Workspace 파일도 지워지지 않도록 모든 하위 항목을 셉니다.
                let q = format!("{} in parents and trashed = false", quote(&f.id));
                if !self.list_page(&q, "1", None).await?.files.is_empty() {
                    return Err(Error::DirectoryNotEmpty);
                }
            }
            (EntryKind::File, false) => {}
        }
        self.trash(&f.id).await?;
        self.forget(path);
        Ok(())
    }

    async fn set_mtime(&self, path: &RemotePath, mtime: SystemTime) -> Result<()> {
        let (f, _, _) = self.visible(path).await?;
        let url = self.api_url(&format!("files/{}", f.id), &[("fields", "id")])?;
        let body = json_body(&serde_json::json!({ "modifiedTime": format_time(mtime)? }));
        self.exec(
            || {
                self.inner
                    .http
                    .patch(url.clone())
                    .header(CONTENT_TYPE, "application/json")
                    .body(body.clone())
            },
            &[],
        )
        .await?;
        Ok(())
    }

    async fn quota(&self) -> Result<Quota> {
        if self.inner.shared_drive.is_some() {
            return Ok(Quota::default());
        }
        let url = self.api_url("about", &[("fields", "storageQuota(limit,usage)")])?;
        let about: About = self.json(|| self.inner.http.get(url.clone())).await?;
        let q = about.storage_quota.unwrap_or(StorageQuota {
            limit: None,
            usage: None,
        });
        let total = q.limit.as_deref().and_then(|v| v.parse::<u64>().ok());
        let used = q.usage.as_deref().and_then(|v| v.parse::<u64>().ok());
        Ok(Quota {
            total,
            used,
            free: total.zip(used).map(|(t, u)| t.saturating_sub(u)),
        })
    }
}

/// 재개 가능 업로드 세션. 순차 쓰기만 받고 8MiB 조각을 모아 보냅니다.
struct DriveUpload {
    backend: GoogleDriveBackend,
    path: RemotePath,
    session: Url,
    buf: BytesMut,
    /// 서버가 받은 바이트 수
    sent: u64,
}

impl DriveUpload {
    /// 조각 하나를 보냅니다. `total`이 있으면 마지막 조각입니다.
    async fn put(&mut self, data: Bytes, total: Option<u64>) -> Result<Response> {
        let len = data.len() as u64;
        let total_text = total.map_or("*".to_owned(), |t| t.to_string());
        let range = if len == 0 {
            format!("bytes */{total_text}")
        } else {
            format!("bytes {}-{}/{total_text}", self.sent, self.sent + len - 1)
        };
        let rb = self
            .backend
            .inner
            .http
            .put(self.session.clone())
            .header(CONTENT_LENGTH, len)
            .header(CONTENT_RANGE, range)
            .body(data);
        let resp = self
            .backend
            .exec_once(rb, &[StatusCode::PERMANENT_REDIRECT])
            .await?;
        if total.is_none() {
            if resp.status() != StatusCode::PERMANENT_REDIRECT {
                return Err(Error::Protocol(
                    "업로드 조각 응답이 올바르지 않습니다".into(),
                ));
            }
            // 서버가 받은 범위(`bytes=0-N`)가 보낸 만큼인지 확인합니다.
            let got = resp
                .headers()
                .get(RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit('-').next())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|last| last + 1);
            if got != Some(self.sent + len) {
                return Err(Error::Protocol(
                    "Google Drive가 업로드 조각 일부만 받았습니다".into(),
                ));
            }
        }
        self.sent += len;
        Ok(resp)
    }
}

#[async_trait]
impl UploadSession for DriveUpload {
    async fn write(&mut self, offset: u64, data: Bytes) -> Result<()> {
        if offset != self.sent + self.buf.len() as u64 {
            return Err(Error::Unsupported("Google Drive 순서 밖 쓰기"));
        }
        self.buf.extend_from_slice(&data);
        while self.buf.len() >= CHUNK {
            let chunk = self.buf.split_to(CHUNK).freeze();
            self.put(chunk, None).await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        // 재개 업로드는 256KiB 배수 조각만 중간에 보낼 수 있습니다. 남은 데이터는 확정할 때 보냅니다.
        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> Result<Entry> {
        let rest = std::mem::take(&mut self.buf).freeze();
        let total = self.sent + rest.len() as u64;
        let resp = self.put(rest, Some(total)).await?;
        if resp.status() == StatusCode::PERMANENT_REDIRECT {
            return Err(Error::Protocol(
                "Google Drive가 업로드를 확정하지 않았습니다".into(),
            ));
        }
        let file: DriveFile = parse_json(resp).await?;
        self.backend.remember_file(&self.path, &file)
    }

    async fn abort(self: Box<Self>) {
        // 세션을 취소합니다(성공하면 499). 실패해도 세션은 일주일 뒤 만료됩니다.
        let rb = self.backend.inner.http.delete(self.session.clone());
        if let Err(e) = rb.send().await {
            tracing::debug!(error = %net_err(e), "Google Drive 업로드 세션 취소 실패");
        }
    }
}

/// 로그인에 쓰는 HTTP 클라이언트 형식(호출자가 reqwest에 직접 의존하지 않게 합니다)
pub type HttpClient = reqwest::Client;
