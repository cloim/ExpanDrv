//! 제어 연결 문자셋 변환과 FTP 응답 코드의 오류 매핑

use expandrv_core::Error;
use secrecy::{ExposeSecret, SecretString};
use std::fmt;
use std::io;
use std::sync::Arc;
use suppaftp::FtpError;

/// `login`에 넘기는 비밀번호 자리표시자. 실제 값은 [`Charset::encode`]에서만 바이트로 바뀌므로
/// suppaftp의 명령 추적 로그에는 자리표시자만 남습니다.
pub(super) const PASS_PLACEHOLDER: &str = "<expandrv-secret>";

/// 설정 문자셋(WHATWG 레이블)으로 명령을 인코딩하고 목록 줄을 디코딩합니다.
#[derive(Clone)]
pub(super) struct Charset {
    enc: &'static encoding_rs::Encoding,
    password: Option<Arc<SecretString>>,
}

impl Charset {
    pub(super) fn new(label: &str) -> Result<Self, Error> {
        let enc = encoding_rs::Encoding::for_label(label.trim().as_bytes())
            .ok_or_else(|| Error::Config(format!("알 수 없는 FTP 문자셋 {label:?}")))?;
        // 명령 동사와 CRLF가 ASCII 그대로여야 하므로 UTF-16, ISO-2022-JP 등은 받지 않습니다.
        if !enc.is_ascii_compatible() {
            return Err(Error::Config(format!(
                "FTP 문자셋 {}는 ASCII 호환이 아닙니다",
                enc.name()
            )));
        }
        Ok(Self {
            enc,
            password: None,
        })
    }

    pub(super) fn with_password(mut self, password: SecretString) -> Self {
        self.password = Some(Arc::new(password));
        self
    }

    pub(super) fn is_utf8(&self) -> bool {
        self.enc == encoding_rs::UTF_8
    }

    fn encode_text(&self, text: &str) -> io::Result<Vec<u8>> {
        let (bytes, _, unmappable) = self.enc.encode(text);
        if unmappable {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} 문자셋으로 표현할 수 없는 문자가 있습니다",
                    self.enc.name()
                ),
            ));
        }
        Ok(bytes.into_owned())
    }
}

impl fmt::Debug for Charset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Charset")
            .field("encoding", &self.enc.name())
            .finish_non_exhaustive()
    }
}

impl suppaftp::Encoding for Charset {
    fn decode(&self, bytes: &[u8]) -> String {
        self.enc.decode_without_bom_handling(bytes).0.into_owned()
    }

    fn encode(&self, text: &str) -> io::Result<Vec<u8>> {
        if let Some(pw) = &self.password
            && text.trim_end_matches("\r\n") == format!("PASS {PASS_PLACEHOLDER}")
        {
            let mut line = b"PASS ".to_vec();
            line.extend(self.encode_text(pw.expose_secret())?);
            line.extend_from_slice(b"\r\n");
            return Ok(line);
        }
        self.encode_text(text)
    }
}

/// 연결 작업 실패. 응답 코드 기반 오류와 연결 상태를 잃은 오류를 구분합니다.
#[derive(Debug)]
pub(super) enum Fail {
    Ftp(FtpError),
    Fs(Error),
    Timeout,
}

impl From<FtpError> for Fail {
    fn from(e: FtpError) -> Self {
        Fail::Ftp(e)
    }
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        Fail::Fs(e)
    }
}

impl Fail {
    /// 서버 응답 코드. 응답 오류가 아니면 `None`입니다.
    pub(super) fn code(&self) -> Option<u32> {
        match self {
            Fail::Ftp(FtpError::UnexpectedResponse(r)) => reply_code(&r.body),
            _ => None,
        }
    }

    /// 제어 연결을 더 쓸 수 없는 실패인지 여부. 이 경우 연결을 풀에 돌려놓지 않습니다.
    pub(super) fn is_broken(&self) -> bool {
        match self {
            Fail::Timeout => true,
            Fail::Fs(_) => false,
            Fail::Ftp(e) => match e {
                FtpError::UnexpectedResponse(r) => reply_code(&r.body) == Some(421),
                _ => true,
            },
        }
    }

    /// 재사용한 연결이 끊겨 있던 경우처럼 새 연결로 한 번 더 시도할 만한 실패인지 여부
    pub(super) fn is_retryable(&self) -> bool {
        match self {
            Fail::Ftp(FtpError::ConnectionError(_)) => true,
            Fail::Ftp(FtpError::UnexpectedResponse(r)) => reply_code(&r.body) == Some(421),
            _ => false,
        }
    }

    /// 서버가 동시 접속 수 제한으로 새 연결을 거부했는지 여부
    pub(super) fn is_capacity_refusal(&self) -> bool {
        let Fail::Ftp(FtpError::UnexpectedResponse(r)) = self else {
            return false;
        };
        let (code, msg) = split_reply(&r.body);
        let msg = msg.to_ascii_lowercase();
        match code {
            Some(421) => true,
            Some(530) => msg.contains("too many") || msg.contains("maximum"),
            _ => false,
        }
    }

    pub(super) fn into_error(self) -> Error {
        match self {
            Fail::Fs(e) => e,
            Fail::Timeout => Error::Timeout,
            Fail::Ftp(e) => ftp_error(e),
        }
    }
}

pub(super) fn ftp_error(e: FtpError) -> Error {
    match e {
        FtpError::ConnectionError(io) if io.kind() == io::ErrorKind::TimedOut => Error::Timeout,
        FtpError::ConnectionError(io) => Error::Network(io.to_string()),
        FtpError::SecureError(s) => Error::Network(format!("TLS: {s}")),
        FtpError::UnexpectedResponse(r) => {
            let (code, msg) = split_reply(&r.body);
            reply_error(code.unwrap_or(0), &msg)
        }
        FtpError::BadResponse => Error::Protocol("FTP 응답 형식이 잘못되었습니다".into()),
        FtpError::InvalidAddress(a) => Error::Network(format!("수동 모드 주소 오류: {a}")),
        FtpError::DataConnectionAlreadyOpen => {
            Error::Protocol("데이터 연결이 이미 열려 있습니다".into())
        }
    }
}

/// 응답 원문 앞 세 자리 코드
pub(super) fn reply_code(body: &[u8]) -> Option<u32> {
    let digits = body.get(..3)?;
    if !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

/// 응답 원문을 코드와 첫 줄 메시지로 나눕니다.
pub(super) fn split_reply(body: &[u8]) -> (Option<u32>, String) {
    let text = String::from_utf8_lossy(body);
    let first = text.lines().next().unwrap_or_default();
    let msg = first.get(4..).unwrap_or_default().trim().to_owned();
    (reply_code(body), msg)
}

pub(super) fn reply_error(code: u32, msg: &str) -> Error {
    let lower = msg.to_ascii_lowercase();
    let denied = [
        "permission denied",
        "access denied",
        "access is denied",
        "not allowed",
    ]
    .iter()
    .any(|k| lower.contains(k));
    match code {
        530 | 430 | 332 | 532 => Error::Auth(format!("{code} {msg}")),
        450 | 550 if denied => Error::PermissionDenied,
        550 if lower.contains("not empty") => Error::DirectoryNotEmpty,
        550 if lower.contains("already exists") || lower.contains("file exists") => {
            Error::AlreadyExists
        }
        550 => Error::NotFound,
        452 | 552 => Error::StorageFull,
        553 => Error::InvalidPath(format!("{code} {msg}")),
        421 | 425 | 426 => Error::Network(format!("{code} {msg}")),
        _ => Error::Protocol(format!("{code} {msg}")),
    }
}
