//! Google OAuth 2.0(설치형 앱): PKCE와 루프백 리다이렉트 로그인, 접근 토큰 갱신
//!
//! 로그인은 `127.0.0.1`의 임의 포트에서 한 번만 리다이렉트를 받고 닫습니다. `state`가 맞지 않거나
//! 코드가 없는 요청(파비콘 등)은 무시하고 다음 요청을 기다립니다.

use expandrv_core::{Error, Result};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// 기존 파일을 읽고 쓰려면 전체 Drive 범위가 필요합니다(drive.file은 앱이 만든 파일만 보입니다).
pub const SCOPE: &str = "https://www.googleapis.com/auth/drive";

/// 브라우저 로그인을 기다리는 최대 시간
pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
/// 연결 하나가 요청 머리글을 보낼 때까지 기다리는 시간
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REQUEST: usize = 16 * 1024;

/// OAuth 클라이언트(Google Cloud 콘솔의 데스크톱 앱 클라이언트)
#[derive(Clone)]
pub struct OAuthClient {
    pub id: String,
    /// 데스크톱 앱 클라이언트의 보안 비밀. 비밀로 취급되지는 않지만 로그에 남기지 않습니다.
    pub secret: Option<SecretString>,
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient")
            .field("id", &self.id)
            .field("secret", &self.secret.as_ref().map(|_| "***"))
            .finish()
    }
}

/// 로그인 결과. 갱신 토큰은 자격 증명 관리자에 저장합니다.
pub struct Authorization {
    pub refresh_token: SecretString,
    pub access_token: SecretString,
    pub expires_in: Duration,
}

/// 접근 토큰과 유효 시간
pub struct AccessToken {
    pub token: SecretString,
    pub expires_in: Duration,
}

/// 길이 `bytes`의 난수를 URL 안전 base64(패딩 없음)로 돌려줍니다.
pub(crate) fn random_token(bytes: usize) -> Result<String> {
    use ring::rand::{SecureRandom, SystemRandom};
    let mut buf = vec![0u8; bytes];
    SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| Error::Io(std::io::Error::other("난수를 만들지 못했습니다")))?;
    Ok(data_encoding::BASE64URL_NOPAD.encode(&buf))
}

/// RFC 7636 S256 코드 챌린지
pub(crate) fn code_challenge(verifier: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    data_encoding::BASE64URL_NOPAD.encode(digest.as_ref())
}

pub(crate) fn authorization_url(
    auth_url: &str,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> Result<String> {
    let url = reqwest::Url::parse_with_params(
        auth_url,
        [
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("response_type", "code"),
            ("scope", SCOPE),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("state", state),
            // 갱신 토큰을 받으려면 오프라인 접근과 동의 화면이 필요합니다.
            ("access_type", "offline"),
            ("prompt", "consent"),
        ],
    )
    .map_err(|e| Error::Config(format!("인증 주소를 만들지 못했습니다: {e}")))?;
    Ok(url.into())
}

/// 기본 브라우저로 주소를 엽니다. 실패해도 호출자가 주소를 직접 안내할 수 있게 오류만 돌려줍니다.
pub fn open_browser(url: &str) -> Result<()> {
    if !url.starts_with("https://") {
        return Err(Error::Config("https 주소만 열 수 있습니다".into()));
    }
    #[cfg(windows)]
    let mut cmd = {
        // cmd /c start는 &를 명령 구분자로 해석하므로 URL 처리기를 직접 부릅니다.
        let mut c = std::process::Command::new("rundll32.exe");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.spawn().map(drop).map_err(Error::from)
}

/// 브라우저 로그인을 진행하고 토큰을 받습니다. `open`은 인증 주소를 사용자에게 보여 주거나 엽니다.
pub async fn sign_in(
    http: &reqwest::Client,
    client: &OAuthClient,
    open: impl FnOnce(&str),
) -> Result<Authorization> {
    sign_in_with(http, client, AUTH_URL, TOKEN_URL, SIGN_IN_TIMEOUT, open).await
}

pub(crate) async fn sign_in_with(
    http: &reqwest::Client,
    client: &OAuthClient,
    auth_url: &str,
    token_url: &str,
    timeout: Duration,
    open: impl FnOnce(&str),
) -> Result<Authorization> {
    if client.id.trim().is_empty() {
        return Err(Error::Config("OAuth 클라이언트 ID를 입력하세요".into()));
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let redirect_uri = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let verifier = random_token(48)?;
    let state = random_token(24)?;
    let url = authorization_url(
        auth_url,
        client.id.trim(),
        &redirect_uri,
        &code_challenge(&verifier),
        &state,
    )?;
    open(&url);
    let code = tokio::time::timeout(timeout, wait_for_code(&listener, &state))
        .await
        .map_err(|_| {
            Error::Auth("Google 로그인 대기 시간이 지났습니다. 다시 시도하세요".into())
        })??;
    drop(listener);
    exchange_code(http, token_url, client, &code, &verifier, &redirect_uri).await
}

/// 리다이렉트 요청 하나의 결과
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Callback {
    Code(String),
    Denied(String),
    /// 코드가 없거나 state가 다른 요청. 무시하고 계속 기다립니다.
    Ignore,
}

/// `GET /?code=..&state=.. HTTP/1.1` 요청 줄을 해석합니다.
pub(crate) fn parse_callback(request_line: &str, state: &str) -> Callback {
    let mut parts = request_line.split_whitespace();
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else {
        return Callback::Ignore;
    };
    let Some((path, query)) = target.split_once('?') else {
        return Callback::Ignore;
    };
    if path != "/" {
        return Callback::Ignore;
    }
    let (mut code, mut error, mut got_state) = (None, None, None);
    for (k, v) in form_urlencoded::parse(query.as_bytes()) {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            "state" => got_state = Some(v.into_owned()),
            _ => {}
        }
    }
    // 다른 페이지가 보낸 요청을 받아들이지 않도록 state를 먼저 확인합니다.
    if got_state.as_deref() != Some(state) {
        return Callback::Ignore;
    }
    match (code, error) {
        (_, Some(e)) => Callback::Denied(e),
        (Some(c), None) if !c.is_empty() => Callback::Code(c),
        _ => Callback::Ignore,
    }
}

async fn wait_for_code(listener: &TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut stream, peer) = listener.accept().await?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let line = match tokio::time::timeout(REQUEST_TIMEOUT, read_request_line(&mut stream)).await
        {
            Ok(Ok(line)) => line,
            _ => continue,
        };
        match parse_callback(&line, state) {
            Callback::Code(code) => {
                respond(
                    &mut stream,
                    200,
                    "로그인했습니다",
                    "ExpanDrv로 돌아가세요. 이 창은 닫아도 됩니다.",
                )
                .await;
                return Ok(code);
            }
            Callback::Denied(e) => {
                respond(
                    &mut stream,
                    200,
                    "로그인하지 않았습니다",
                    "ExpanDrv에서 다시 시도할 수 있습니다. 이 창은 닫아도 됩니다.",
                )
                .await;
                return Err(Error::Auth(if e == "access_denied" {
                    "Google 계정 접근을 허용하지 않았습니다".into()
                } else {
                    format!("Google 로그인 오류: {e}")
                }));
            }
            Callback::Ignore => respond(&mut stream, 404, "찾을 수 없습니다", "").await,
        }
    }
}

/// 머리글 끝까지 읽고 첫 줄을 돌려줍니다.
async fn read_request_line(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await?;
        if n == 0 || buf.len() + n > MAX_REQUEST {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let text = String::from_utf8_lossy(&buf);
    Ok(text.lines().next().unwrap_or_default().to_owned())
}

async fn respond(stream: &mut TcpStream, status: u16, title: &str, body: &str) {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let html = format!(
        "<!doctype html><html lang=\"ko\"><head><meta charset=\"utf-8\"><title>ExpanDrv</title>\
         <style>body{{font-family:'Segoe UI',sans-serif;margin:15vh auto;max-width:28rem;padding:0 16px;color:#222}}\
         @media(prefers-color-scheme:dark){{body{{background:#1b1b1b;color:#eee}}}}</style></head>\
         <body><h1>{title}</h1><p>{body}</p></body></html>"
    );
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        html.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(html.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

fn form_body(pairs: &[(&str, &str)]) -> String {
    let mut s = form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

async fn token_request(
    http: &reqwest::Client,
    token_url: &str,
    client: &OAuthClient,
    mut pairs: Vec<(&str, &str)>,
) -> Result<TokenResponse> {
    pairs.push(("client_id", client.id.trim()));
    if let Some(s) = &client.secret {
        pairs.push(("client_secret", s.expose_secret()));
    }
    let resp = http
        .post(token_url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form_body(&pairs))
        .send()
        .await
        .map_err(super::net_err)?;
    let status = resp.status();
    let text = resp.text().await.map_err(super::net_err)?;
    if status.is_success() {
        return serde_json::from_str(&text)
            .map_err(|e| Error::Protocol(format!("토큰 응답을 해석하지 못했습니다: {e}")));
    }
    Err(token_error(status.as_u16(), &text))
}

/// 토큰 엔드포인트 오류를 사용자에게 보일 오류로 바꿉니다.
pub(crate) fn token_error(status: u16, body: &str) -> Error {
    let Ok(e) = serde_json::from_str::<TokenError>(body) else {
        return if status >= 500 {
            Error::Network(format!("Google 인증 서버 오류(HTTP {status})"))
        } else {
            Error::Protocol(format!("Google 인증 서버 응답 HTTP {status}"))
        };
    };
    let detail = e.error_description.unwrap_or_default();
    match e.error.as_str() {
        "invalid_grant" => Error::Auth(
            "Google 로그인이 만료되었거나 취소되었습니다. 드라이브 속성에서 다시 로그인하세요"
                .into(),
        ),
        "invalid_client" | "unauthorized_client" => Error::Auth(format!(
            "OAuth 클라이언트 ID 또는 보안 비밀이 올바르지 않습니다({detail})"
        )),
        _ if status >= 500 => Error::Network(format!("Google 인증 서버 오류: {}", e.error)),
        other => Error::Auth(
            format!("Google 인증 오류: {other} {detail}")
                .trim()
                .to_owned(),
        ),
    }
}

pub(crate) async fn exchange_code(
    http: &reqwest::Client,
    token_url: &str,
    client: &OAuthClient,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Authorization> {
    let r = token_request(
        http,
        token_url,
        client,
        vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect_uri),
        ],
    )
    .await?;
    let refresh_token = r.refresh_token.filter(|t| !t.is_empty()).ok_or_else(|| {
        Error::Auth("Google이 갱신 토큰을 주지 않았습니다. 다시 로그인하세요".into())
    })?;
    Ok(Authorization {
        refresh_token: SecretString::from(refresh_token),
        access_token: SecretString::from(r.access_token),
        expires_in: Duration::from_secs(r.expires_in.unwrap_or(3600)),
    })
}

pub(crate) async fn refresh(
    http: &reqwest::Client,
    token_url: &str,
    client: &OAuthClient,
    refresh_token: &SecretString,
) -> Result<AccessToken> {
    let r = token_request(
        http,
        token_url,
        client,
        vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.expose_secret()),
        ],
    )
    .await?;
    Ok(AccessToken {
        token: SecretString::from(r.access_token),
        expires_in: Duration::from_secs(r.expires_in.unwrap_or(3600)),
    })
}
