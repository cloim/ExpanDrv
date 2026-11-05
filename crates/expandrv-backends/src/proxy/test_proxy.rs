//! 테스트용 최소 SOCKS5와 HTTP CONNECT 프록시. 연결 대상을 기록하고 양방향으로 중계합니다.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Debug, Clone, Copy)]
pub enum ProxyMode {
    Socks5,
    Http,
}

pub struct FakeProxy {
    pub port: u16,
    targets: Arc<Mutex<Vec<String>>>,
}

impl FakeProxy {
    pub async fn start(mode: ProxyMode, auth: Option<(&str, &str)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let targets = Arc::new(Mutex::new(Vec::new()));
        let auth = auth.map(|(u, p)| (u.to_owned(), p.to_owned()));
        let rec = targets.clone();
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let (rec, auth) = (rec.clone(), auth.clone());
                tokio::spawn(async move {
                    let _ = match mode {
                        ProxyMode::Socks5 => socks5(client, auth, rec).await,
                        ProxyMode::Http => http(client, auth, rec).await,
                    };
                });
            }
        });
        Self { port, targets }
    }

    pub fn targets(&self) -> Vec<String> {
        self.targets.lock().unwrap().clone()
    }
}

async fn relay(mut client: TcpStream, target: &str) -> std::io::Result<()> {
    let mut upstream = TcpStream::connect(target).await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

async fn socks5(
    mut c: TcpStream,
    auth: Option<(String, String)>,
    rec: Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    let mut head = [0u8; 2];
    c.read_exact(&mut head).await?;
    let mut methods = vec![0u8; head[1] as usize];
    c.read_exact(&mut methods).await?;
    match &auth {
        Some((user, pass)) => {
            if !methods.contains(&2) {
                return c.write_all(&[5, 0xFF]).await;
            }
            c.write_all(&[5, 2]).await?;
            let mut v = [0u8; 2];
            c.read_exact(&mut v).await?;
            let mut u = vec![0u8; v[1] as usize];
            c.read_exact(&mut u).await?;
            let mut pl = [0u8; 1];
            c.read_exact(&mut pl).await?;
            let mut p = vec![0u8; pl[0] as usize];
            c.read_exact(&mut p).await?;
            let ok = u == user.as_bytes() && p == pass.as_bytes();
            c.write_all(&[1, if ok { 0 } else { 1 }]).await?;
            if !ok {
                return Ok(());
            }
        }
        None => c.write_all(&[5, 0]).await?,
    }
    let mut req = [0u8; 4];
    c.read_exact(&mut req).await?;
    let host = match req[3] {
        1 => {
            let mut a = [0u8; 4];
            c.read_exact(&mut a).await?;
            Ipv4Addr::from(a).to_string()
        }
        4 => {
            let mut a = [0u8; 16];
            c.read_exact(&mut a).await?;
            format!("[{}]", Ipv6Addr::from(a))
        }
        _ => {
            let mut l = [0u8; 1];
            c.read_exact(&mut l).await?;
            let mut n = vec![0u8; l[0] as usize];
            c.read_exact(&mut n).await?;
            String::from_utf8_lossy(&n).into_owned()
        }
    };
    let mut port = [0u8; 2];
    c.read_exact(&mut port).await?;
    let target = format!("{host}:{}", u16::from_be_bytes(port));
    rec.lock().unwrap().push(target.clone());
    // 테스트 서버는 IPv4 루프백에서만 대기하므로 루프백 대상은 127.0.0.1로 중계합니다.
    let connect_to = target
        .replace("localhost:", "127.0.0.1:")
        .replace("[::1]:", "127.0.0.1:");
    c.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    relay(c, &connect_to).await
}

async fn http(
    mut c: TcpStream,
    auth: Option<(String, String)>,
    rec: Arc<Mutex<Vec<String>>>,
) -> std::io::Result<()> {
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if c.read(&mut b).await? == 0 {
            return Ok(());
        }
        head.push(b[0]);
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let target = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_owned();
    if let Some((user, pass)) = &auth {
        let expected = format!(
            "proxy-authorization: basic {}",
            super::base64(format!("{user}:{pass}").as_bytes())
        )
        .to_lowercase();
        if !text.to_lowercase().lines().any(|l| l.trim() == expected) {
            return c
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n",
                )
                .await;
        }
    }
    rec.lock().unwrap().push(target.clone());
    c.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await?;
    relay(c, &target.replace("localhost:", "127.0.0.1:")).await
}
