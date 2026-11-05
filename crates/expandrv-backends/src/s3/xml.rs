//! S3 XML 응답 파싱, 요청 본문 생성과 HTTP 상태 오류 매핑

use expandrv_core::{Error, Result};
use quick_xml::events::Event;
use serde::Deserialize;
use std::fmt::Write as _;

/// ListObjectsV2 응답 한 페이지
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct ListPage {
    #[serde(default)]
    pub contents: Vec<ObjectItem>,
    #[serde(default)]
    pub common_prefixes: Vec<PrefixItem>,
    #[serde(default)]
    pub is_truncated: bool,
    pub next_continuation_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct ObjectItem {
    pub key: String,
    #[serde(default)]
    pub size: u64,
    pub last_modified: Option<String>,
    #[serde(rename = "ETag")]
    pub etag: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct PrefixItem {
    pub prefix: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct InitiateResult {
    upload_id: String,
}

#[derive(Debug, Deserialize)]
struct CopyResult {
    #[serde(rename = "ETag")]
    etag: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ErrorBody {
    code: Option<String>,
    message: Option<String>,
}

fn de<'a, T: Deserialize<'a>>(text: &'a str, what: &str) -> Result<T> {
    quick_xml::de::from_str(text)
        .map_err(|e| Error::Protocol(format!("{what} 응답 해석 실패: {e}")))
}

pub(super) fn parse_list(text: &str) -> Result<ListPage> {
    de(text, "ListObjectsV2")
}

pub(super) fn parse_upload_id(text: &str) -> Result<String> {
    let r: InitiateResult = de(text, "CreateMultipartUpload")?;
    if r.upload_id.is_empty() {
        return Err(Error::Protocol("UploadId가 비어 있습니다".into()));
    }
    Ok(r.upload_id)
}

/// CopyPartResult 또는 CopyObjectResult의 ETag
pub(super) fn parse_copy_etag(text: &str) -> Result<String> {
    let r: CopyResult = de(text, "UploadPartCopy")?;
    r.etag
        .filter(|e| !e.is_empty())
        .ok_or_else(|| Error::Protocol("복사 응답에 ETag가 없습니다".into()))
}

/// 최상위 요소 이름. 200 응답 본문에 `<Error>`가 오는 경우(CopyObject, CompleteMultipartUpload) 판별에 씁니다.
pub(super) fn root_name(text: &str) -> Option<String> {
    let mut reader = quick_xml::Reader::from_str(text);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) => {
                return Some(String::from_utf8_lossy(e.local_name().as_ref()).into_owned());
            }
            Ok(Event::Eof) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

/// 오류 응답 본문에서 `Code`와 `Message`를 꺼냅니다. HEAD처럼 본문이 없으면 둘 다 `None`입니다.
pub(super) fn parse_error(text: &str) -> (Option<String>, Option<String>) {
    if root_name(text).as_deref() != Some("Error") {
        return (None, None);
    }
    let body: ErrorBody = quick_xml::de::from_str(text).unwrap_or_default();
    (body.code, body.message)
}

pub(super) fn complete_body(parts: &[(u32, String)]) -> String {
    let mut s = String::from("<CompleteMultipartUpload>");
    for (n, etag) in parts {
        let _ = write!(
            s,
            "<Part><PartNumber>{n}</PartNumber><ETag>{}</ETag></Part>",
            quick_xml::escape::escape(etag.as_str())
        );
    }
    s.push_str("</CompleteMultipartUpload>");
    s
}

/// HTTP 상태와 S3 오류 코드를 공통 오류로 바꿉니다. 메시지에 서명 URL이나 자격 증명을 넣지 않습니다.
pub(super) fn map_status(status: u16, code: Option<&str>, message: Option<&str>) -> Error {
    let c = code.unwrap_or("");
    match (status, c) {
        (_, "NoSuchBucket") => Error::Config("버킷이 없습니다".into()),
        (
            _,
            "PermanentRedirect"
            | "AuthorizationHeaderMalformed"
            | "IllegalLocationConstraintException",
        ) => Error::Config(format!("엔드포인트 또는 리전 설정을 확인하세요({c})")),
        (_, "NoSuchKey" | "NoSuchUpload") | (404, _) => Error::NotFound,
        (_, "RequestTimeTooSkewed") => Error::Auth("PC 시계와 서버 시각 차이가 너무 큽니다".into()),
        (
            _,
            "InvalidAccessKeyId"
            | "SignatureDoesNotMatch"
            | "ExpiredToken"
            | "InvalidToken"
            | "TokenRefreshRequired"
            | "InvalidSecurity",
        )
        | (401, _) => Error::Auth(if c.is_empty() {
            "자격 증명이 거부되었습니다".into()
        } else {
            c.to_owned()
        }),
        (403, _) | (_, "AccessDenied" | "AllAccessDisabled") => Error::PermissionDenied,
        (_, "QuotaExceeded" | "EntityTooLarge") | (507, _) => Error::StorageFull,
        (_, c) if c.contains("StorageFull") => Error::StorageFull,
        (_, "RequestTimeout") | (408, _) => Error::Timeout,
        (_, "SlowDown" | "ServiceUnavailable" | "InternalError") | (500..=599, _) => {
            Error::Network(format!("서버 오류 HTTP {status} {c}").trim_end().to_owned())
        }
        _ => Error::Protocol(match message {
            Some(m) if !m.is_empty() => format!("HTTP {status} {c}: {m}"),
            _ => format!("HTTP {status} {c}").trim_end().to_owned(),
        }),
    }
}
