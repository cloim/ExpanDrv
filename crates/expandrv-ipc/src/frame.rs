//! u32 LE 길이 접두 + MessagePack 본문 프레임

use crate::{Error, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 본문 최대 크기(양방향)
pub const MAX_FRAME: usize = 1 << 20;

pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize + ?Sized,
{
    let body = rmp_serde::to_vec_named(msg).map_err(|e| Error::Codec(e.to_string()))?;
    if body.len() > MAX_FRAME {
        return Err(Error::FrameTooLarge(body.len()));
    }
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

/// 프레임 경계에서 상대가 닫으면 `None`을 돌려줍니다.
pub async fn read_frame<R, T>(r: &mut R) -> Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len = [0u8; 4];
    let mut got = 0;
    while got < len.len() {
        match r.read(&mut len[got..]).await {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(Error::Closed),
            Ok(n) => got += n,
            // 상대가 파이프를 끊으면 EOF 대신 BrokenPipe가 옵니다.
            Err(e) if got == 0 && e.kind() == std::io::ErrorKind::BrokenPipe => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(Error::FrameTooLarge(len));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await.map_err(|e| match e.kind() {
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe => Error::Closed,
        _ => e.into(),
    })?;
    rmp_serde::from_slice(&body)
        .map(Some)
        .map_err(|e| Error::Codec(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentStatus, DriveState, DriveStatus, ErrorCode, Request, Response};

    #[tokio::test]
    async fn frame_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let reqs = vec![
            Request::Hello { version: 1 },
            Request::Status,
            Request::Mount { id: "nas".into() },
            Request::Unmount { id: "nas".into() },
            Request::Reload,
            Request::Shutdown,
        ];
        let resp = Response::Status(AgentStatus {
            drives: vec![DriveStatus {
                id: "nas".into(),
                label: "NAS".into(),
                mount_point: "W:".into(),
                backend: "webdav".into(),
                state: DriveState::Failed("연결 실패".into()),
                pending_uploads: 3,
            }],
        });
        let sent = reqs.clone();
        let resp2 = resp.clone();
        let writer = tokio::spawn(async move {
            for r in &sent {
                write_frame(&mut a, r).await.unwrap();
            }
            write_frame(&mut a, &resp2).await.unwrap();
        });
        for want in &reqs {
            let got: Request = read_frame(&mut b).await.unwrap().unwrap();
            assert_eq!(&got, want);
        }
        let got: Response = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(got, resp);
        writer.await.unwrap();
        // 경계에서 닫히면 None
        assert!(read_frame::<_, Request>(&mut b).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn oversize_frames_are_rejected() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let big = Response::error(ErrorCode::Internal, "x".repeat(MAX_FRAME + 1));
        assert!(matches!(
            write_frame(&mut a, &big).await,
            Err(Error::FrameTooLarge(_))
        ));

        // 길이 접두만 큰 값으로 보내도 본문을 읽기 전에 거부합니다.
        a.write_all(&((MAX_FRAME as u32) + 1).to_le_bytes())
            .await
            .unwrap();
        assert!(matches!(
            read_frame::<_, Request>(&mut b).await,
            Err(Error::FrameTooLarge(n)) if n == MAX_FRAME + 1
        ));
    }

    #[tokio::test]
    async fn truncated_and_garbage_frames_fail() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&3u32.to_le_bytes()).await.unwrap();
        a.write_all(&[0xc1, 0xc1, 0xc1]).await.unwrap();
        a.write_all(&10u32.to_le_bytes()).await.unwrap();
        a.write_all(b"abc").await.unwrap();
        drop(a);
        assert!(matches!(
            read_frame::<_, Request>(&mut b).await,
            Err(Error::Codec(_))
        ));
        assert!(matches!(
            read_frame::<_, Request>(&mut b).await,
            Err(Error::Closed)
        ));
    }
}
