//! SOCKS5(RFC 1928, 사용자 인증 RFC 1929)와 HTTP CONNECT 프록시 터널.
//!
//! SFTP와 FTP는 이 모듈로 연 TCP 스트림을 그대로 쓰고, HTTP 계열(WebDAV, S3)은 같은 설정을
//! reqwest 프록시로 바꿔 씁니다. 자격 증명은 메시지와 로그에 남기지 않습니다.

use expandrv_core::config::{ProxyConfig, SecretResolver};
use expandrv_core::{Error, Result};
use secrecy::{ExposeSecret, SecretString};
use std::io;
use std::net::IpAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use zeroize::Zeroizing;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// HTTP CONNECT 응답 헤더 상한
const MAX_HTTP_HEADER: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `remote_dns`면 대상 이름을 프록시가 해석합니다(socks5h).
    Socks5 {
        remote_dns: bool,
    },
    Http,
}

pub struct Proxy {
    kind: Kind,
    host: String,
    port: u16,
    auth: Option<(String, SecretString)>,
}

impl std::fmt::Debug for Proxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proxy")
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("auth", &self.auth.as_ref().map(|(u, _)| u))
            .finish()
    }
}

fn proxy_err(msg: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("프록시: {msg}"))
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for c in input.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

impl Proxy {
    pub fn from_config(cfg: &ProxyConfig, secrets: &dyn SecretResolver) -> Result<Self> {
        let (scheme, host, port) = cfg.parse()?;
        let kind = match scheme {
            "socks5" => Kind::Socks5 { remote_dns: false },
            "socks5h" => Kind::Socks5 { remote_dns: true },
            _ => Kind::Http,
        };
        let auth = match (&cfg.username, &cfg.password) {
            (Some(u), Some(p)) => Some((u.clone(), secrets.resolve(p)?)),
            (Some(u), None) => Some((u.clone(), SecretString::from(""))),
            (None, Some(_)) => {
                return Err(Error::Config(
                    "프록시 password에는 username이 필요합니다".into(),
                ));
            }
            (None, None) => None,
        };
        if let (Kind::Socks5 { .. }, Some((u, p))) = (kind, &auth)
            && (u.len() > 255 || p.expose_secret().len() > 255)
        {
            return Err(Error::Config(
                "SOCKS5 자격 증명은 255바이트 이하여야 합니다".into(),
            ));
        }
        Ok(Self {
            kind,
            host: host.to_owned(),
            port,
            auth,
        })
    }

    /// 프록시를 거쳐 `host:port`로 가는 TCP 스트림을 엽니다.
    pub async fn connect(&self, host: &str, port: u16) -> io::Result<TcpStream> {
        tokio::time::timeout(CONNECT_TIMEOUT, self.connect_inner(host, port))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "프록시 연결 시간 초과"))?
    }

    async fn connect_inner(&self, host: &str, port: u16) -> io::Result<TcpStream> {
        let mut s = TcpStream::connect((self.host.as_str(), self.port)).await?;
        s.set_nodelay(true)?;
        match self.kind {
            Kind::Socks5 { remote_dns } => self.socks5(&mut s, host, port, remote_dns).await?,
            Kind::Http => self.http_connect(&mut s, host, port).await?,
        }
        Ok(s)
    }

    async fn socks5(
        &self,
        s: &mut TcpStream,
        host: &str,
        port: u16,
        remote_dns: bool,
    ) -> io::Result<()> {
        let greeting: &[u8] = if self.auth.is_some() {
            &[5, 2, 0, 2]
        } else {
            &[5, 1, 0]
        };
        s.write_all(greeting).await?;
        let mut reply = [0u8; 2];
        s.read_exact(&mut reply).await?;
        if reply[0] != 5 {
            return Err(proxy_err("SOCKS5 서버가 아닙니다"));
        }
        match (reply[1], &self.auth) {
            (0, _) => {}
            (2, Some((user, pass))) => {
                let pass = pass.expose_secret().as_bytes();
                let mut msg = Zeroizing::new(Vec::with_capacity(3 + user.len() + pass.len()));
                msg.push(1);
                msg.push(user.len() as u8);
                msg.extend_from_slice(user.as_bytes());
                msg.push(pass.len() as u8);
                msg.extend_from_slice(pass);
                s.write_all(&msg).await?;
                let mut st = [0u8; 2];
                s.read_exact(&mut st).await?;
                if st[1] != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "SOCKS5 인증 실패",
                    ));
                }
            }
            (0xFF, _) | (2, None) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "SOCKS5 서버가 인증 방식을 거부했습니다",
                ));
            }
            (m, _) => return Err(proxy_err(format!("지원하지 않는 SOCKS5 인증 방식 {m}"))),
        }

        let mut req = vec![5, 1, 0];
        let ip = match host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
        {
            Ok(ip) => Some(ip),
            Err(_) if remote_dns => None,
            Err(_) => Some(
                tokio::net::lookup_host((host, port))
                    .await?
                    .next()
                    .ok_or_else(|| proxy_err(format!("{host} 이름을 해석할 수 없습니다")))?
                    .ip(),
            ),
        };
        match ip {
            Some(IpAddr::V4(v4)) => {
                req.push(1);
                req.extend_from_slice(&v4.octets());
            }
            Some(IpAddr::V6(v6)) => {
                req.push(4);
                req.extend_from_slice(&v6.octets());
            }
            None => {
                let name = host.as_bytes();
                if name.len() > 255 {
                    return Err(proxy_err("호스트 이름이 너무 깁니다"));
                }
                req.push(3);
                req.push(name.len() as u8);
                req.extend_from_slice(name);
            }
        }
        req.extend_from_slice(&port.to_be_bytes());
        s.write_all(&req).await?;

        let mut head = [0u8; 4];
        s.read_exact(&mut head).await?;
        if head[1] != 0 {
            let why = match head[1] {
                1 => "일반 실패",
                2 => "규칙에 의해 거부됨",
                3 => "네트워크에 도달할 수 없음",
                4 => "호스트에 도달할 수 없음",
                5 => "연결 거부됨",
                6 => "TTL 만료",
                7 => "지원하지 않는 명령",
                8 => "지원하지 않는 주소 형식",
                _ => "알 수 없는 오류",
            };
            return Err(proxy_err(format!("SOCKS5 연결 실패: {why}")));
        }
        let skip = match head[3] {
            1 => 4,
            4 => 16,
            3 => {
                let mut len = [0u8; 1];
                s.read_exact(&mut len).await?;
                len[0] as usize
            }
            t => return Err(proxy_err(format!("SOCKS5 응답 주소 형식 {t}"))),
        };
        let mut rest = vec![0u8; skip + 2];
        s.read_exact(&mut rest).await?;
        Ok(())
    }

    async fn http_connect(&self, s: &mut TcpStream, host: &str, port: u16) -> io::Result<()> {
        let authority = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let mut req = Zeroizing::new(format!(
            "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n"
        ));
        if let Some((user, pass)) = &self.auth {
            let cred = Zeroizing::new(format!("{user}:{}", pass.expose_secret()));
            req.push_str("Proxy-Authorization: Basic ");
            req.push_str(&base64(cred.as_bytes()));
            req.push_str("\r\n");
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes()).await?;

        // 터널 뒤 데이터(SSH 배너 등)를 삼키지 않도록 헤더 끝까지 한 바이트씩 읽습니다.
        let mut head = Vec::with_capacity(256);
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if head.len() >= MAX_HTTP_HEADER {
                return Err(proxy_err("HTTP CONNECT 응답 헤더가 너무 깁니다"));
            }
            if s.read(&mut b).await? == 0 {
                return Err(proxy_err("HTTP 프록시가 연결을 닫았습니다"));
            }
            head.push(b[0]);
        }
        let status_line = String::from_utf8_lossy(&head);
        let status_line = status_line.lines().next().unwrap_or_default();
        let code = status_line.split_whitespace().nth(1).unwrap_or_default();
        match code {
            "200" => Ok(()),
            "407" => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "HTTP 프록시 인증 실패(407)",
            )),
            _ => Err(proxy_err(format!("HTTP CONNECT 실패: {status_line}"))),
        }
    }

    /// reqwest용 프록시 설정. HTTP 계열 백엔드에서 씁니다.
    #[cfg(any(feature = "webdav", feature = "s3"))]
    pub(crate) fn reqwest(&self) -> Result<reqwest::Proxy> {
        let scheme = match self.kind {
            Kind::Socks5 { remote_dns: false } => "socks5",
            Kind::Socks5 { remote_dns: true } => "socks5h",
            Kind::Http => "http",
        };
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let mut p = reqwest::Proxy::all(format!("{scheme}://{host}:{}", self.port))
            .map_err(|e| Error::Config(format!("프록시 설정 실패: {e}")))?;
        if let Some((user, pass)) = &self.auth {
            p = p.basic_auth(user, pass.expose_secret());
        }
        Ok(p)
    }
}

#[cfg(test)]
pub(crate) mod test_proxy;

#[cfg(test)]
mod backend_tests;

#[cfg(test)]
mod tests {
    use super::test_proxy::{FakeProxy, ProxyMode};
    use super::*;
    use expandrv_core::config::{EnvSecrets, SecretSource};

    fn cfg(url: String, auth: bool) -> ProxyConfig {
        ProxyConfig {
            url,
            username: auth.then(|| "user".to_owned()),
            password: auth.then(|| SecretSource::Env {
                var: "EXPANDRV_TEST_PROXY_PASS".into(),
            }),
        }
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (i, o) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
        ] {
            assert_eq!(base64(i.as_bytes()), o);
        }
    }

    #[test]
    fn debug_hides_password() {
        // SAFETY: 이 테스트만 쓰는 고유 변수입니다.
        unsafe { std::env::set_var("EXPANDRV_TEST_PROXY_PASS", "s3cret-pw") };
        let p =
            Proxy::from_config(&cfg("socks5://127.0.0.1:1080".into(), true), &EnvSecrets).unwrap();
        assert!(!format!("{p:?}").contains("s3cret-pw"));
    }

    async fn roundtrip(
        mode: ProxyMode,
        scheme: &str,
        target_host: &str,
        auth: bool,
    ) -> (Vec<String>, String) {
        // SAFETY: 테스트 전용 고유 변수이며 모든 테스트가 같은 값을 씁니다.
        unsafe { std::env::set_var("EXPANDRV_TEST_PROXY_PASS", "s3cret-pw") };
        let proxy = FakeProxy::start(mode, auth.then_some(("user", "s3cret-pw"))).await;
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut c, _) = echo.accept().await.unwrap();
            // 서버가 먼저 말하는 프로토콜(SSH 배너)을 흉내 냅니다.
            c.write_all(b"HELLO\n").await.unwrap();
            let mut buf = [0u8; 4];
            c.read_exact(&mut buf).await.unwrap();
            c.write_all(&buf).await.unwrap();
        });
        let p = Proxy::from_config(
            &cfg(format!("{scheme}://127.0.0.1:{}", proxy.port), auth),
            &EnvSecrets,
        )
        .unwrap();
        let mut s = p.connect(target_host, echo_port).await.unwrap();
        let mut hello = [0u8; 6];
        s.read_exact(&mut hello).await.unwrap();
        s.write_all(b"ping").await.unwrap();
        let mut back = [0u8; 4];
        s.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"ping");
        (
            proxy.targets(),
            String::from_utf8_lossy(&hello).into_owned(),
        )
    }

    #[tokio::test]
    async fn socks5_with_auth_tunnels_and_resolves_locally() {
        let (targets, hello) = roundtrip(ProxyMode::Socks5, "socks5", "localhost", true).await;
        assert_eq!(hello, "HELLO\n");
        assert!(
            targets[0].starts_with("127.0.0.1:") || targets[0].starts_with("[::1]:"),
            "{targets:?}"
        );
    }

    #[tokio::test]
    async fn socks5h_sends_domain_name() {
        let (targets, _) = roundtrip(ProxyMode::Socks5, "socks5h", "localhost", false).await;
        assert!(targets[0].starts_with("localhost:"), "{targets:?}");
    }

    #[tokio::test]
    async fn http_connect_with_basic_auth_keeps_early_server_bytes() {
        let (targets, hello) = roundtrip(ProxyMode::Http, "http", "127.0.0.1", true).await;
        assert_eq!(hello, "HELLO\n");
        assert!(targets[0].starts_with("127.0.0.1:"));
    }

    #[tokio::test]
    async fn wrong_credentials_are_permission_denied() {
        for (mode, scheme) in [(ProxyMode::Socks5, "socks5"), (ProxyMode::Http, "http")] {
            let proxy = FakeProxy::start(mode, Some(("user", "other"))).await;
            unsafe { std::env::set_var("EXPANDRV_TEST_PROXY_PASS", "s3cret-pw") };
            let p = Proxy::from_config(
                &cfg(format!("{scheme}://127.0.0.1:{}", proxy.port), true),
                &EnvSecrets,
            )
            .unwrap();
            let e = p.connect("127.0.0.1", 9).await.unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{scheme}: {e}");
            assert!(!e.to_string().contains("s3cret-pw"));
        }
    }
}
