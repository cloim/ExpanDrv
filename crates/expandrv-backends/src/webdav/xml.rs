//! PROPFIND 다중 상태(207) 응답 해석. 서버마다 접두사(`D:`, `d:`, 기본 네임스페이스)가 달라
//! 네임스페이스로 판별하고, 바인딩이 없는 요소도 DAV 요소로 받아들입니다.

use percent_encoding::percent_decode_str;
use quick_xml::NsReader;
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{Namespace, ResolveResult};
use std::time::SystemTime;

pub(super) const PROPFIND_STAT: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<D:propfind xmlns:D="DAV:"><D:prop>"#,
    "<D:resourcetype/><D:getcontentlength/><D:getlastmodified/>",
    "<D:creationdate/><D:getetag/>",
    "</D:prop></D:propfind>"
);

pub(super) const PROPFIND_QUOTA: &str = concat!(
    r#"<?xml version="1.0" encoding="utf-8"?>"#,
    r#"<D:propfind xmlns:D="DAV:"><D:prop>"#,
    "<D:quota-available-bytes/><D:quota-used-bytes/>",
    "</D:prop></D:propfind>"
);

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Props {
    pub collection: bool,
    pub content_length: Option<u64>,
    pub last_modified: Option<SystemTime>,
    pub creation_date: Option<SystemTime>,
    pub etag: Option<String>,
    pub quota_available: Option<u64>,
    pub quota_used: Option<u64>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct DavResponse {
    /// 엔티티를 풀었지만 퍼센트 인코딩은 유지한 원본 href
    pub href: String,
    /// `response` 직속 `status`가 2xx가 아니면 false(항목 없음 등)
    pub ok: bool,
    pub props: Props,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Response,
    Href,
    Propstat,
    Prop,
    Status,
    ResourceType,
    Collection,
    ContentLength,
    LastModified,
    CreationDate,
    Etag,
    QuotaAvailable,
    QuotaUsed,
    Other,
}

fn tag_of(ns: &ResolveResult<'_>, local: &[u8]) -> Tag {
    let dav = match ns {
        ResolveResult::Bound(Namespace(n)) => *n == b"DAV:",
        ResolveResult::Unbound => true,
        ResolveResult::Unknown(_) => false,
    };
    if !dav {
        return Tag::Other;
    }
    match local {
        b"response" => Tag::Response,
        b"href" => Tag::Href,
        b"propstat" => Tag::Propstat,
        b"prop" => Tag::Prop,
        b"status" => Tag::Status,
        b"resourcetype" => Tag::ResourceType,
        b"collection" => Tag::Collection,
        b"getcontentlength" => Tag::ContentLength,
        b"getlastmodified" => Tag::LastModified,
        b"creationdate" => Tag::CreationDate,
        b"getetag" => Tag::Etag,
        b"quota-available-bytes" => Tag::QuotaAvailable,
        b"quota-used-bytes" => Tag::QuotaUsed,
        _ => Tag::Other,
    }
}

/// `HTTP/1.1 200 OK`에서 상태 코드가 2xx인지 봅니다. 해석할 수 없으면 성공으로 간주합니다.
fn status_ok(line: &str) -> bool {
    line.split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .is_none_or(|c| (200..300).contains(&c))
}

pub(super) fn parse_http_date(s: &str) -> Option<SystemTime> {
    httpdate::parse_http_date(s.trim()).ok()
}

/// RFC 3339(`creationdate` 표준 형식)를 우선하고, HTTP 날짜를 쓰는 서버도 받아들입니다.
pub(super) fn parse_creation_date(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(SystemTime::from)
        .or_else(|| parse_http_date(s))
}

#[derive(Default)]
struct State {
    stack: Vec<Tag>,
    text: String,
    cur: Option<DavResponse>,
    ps_props: Props,
    ps_status: Option<String>,
    out: Vec<DavResponse>,
}

impl State {
    fn parent(&self) -> Option<Tag> {
        self.stack.last().copied()
    }

    fn open(&mut self, tag: Tag) {
        match tag {
            Tag::Response => {
                self.cur = Some(DavResponse {
                    ok: true,
                    ..DavResponse::default()
                });
            }
            Tag::Propstat => {
                self.ps_props = Props::default();
                self.ps_status = None;
            }
            Tag::Collection if self.parent() == Some(Tag::ResourceType) => {
                self.ps_props.collection = true;
            }
            _ => {}
        }
        self.stack.push(tag);
        self.text.clear();
    }

    fn close(&mut self) {
        let Some(tag) = self.stack.pop() else {
            return;
        };
        let text = std::mem::take(&mut self.text);
        let text = text.trim();
        let parent = self.parent();
        let in_prop = parent == Some(Tag::Prop);
        match tag {
            Tag::Href if parent == Some(Tag::Response) => {
                if let Some(r) = self.cur.as_mut() {
                    r.href = text.to_owned();
                }
            }
            Tag::Status if parent == Some(Tag::Propstat) => self.ps_status = Some(text.to_owned()),
            Tag::Status if parent == Some(Tag::Response) => {
                if let Some(r) = self.cur.as_mut() {
                    r.ok = status_ok(text);
                }
            }
            Tag::ContentLength if in_prop => self.ps_props.content_length = text.parse().ok(),
            Tag::LastModified if in_prop => self.ps_props.last_modified = parse_http_date(text),
            Tag::CreationDate if in_prop => self.ps_props.creation_date = parse_creation_date(text),
            Tag::Etag if in_prop && !text.is_empty() => self.ps_props.etag = Some(text.to_owned()),
            // Nextcloud 등은 무제한(-3)이나 알 수 없음(-2)을 음수로 알립니다.
            Tag::QuotaAvailable if in_prop => self.ps_props.quota_available = text.parse().ok(),
            Tag::QuotaUsed if in_prop => self.ps_props.quota_used = text.parse().ok(),
            Tag::Propstat => {
                if self.ps_status.as_deref().is_none_or(status_ok) {
                    let ps = std::mem::take(&mut self.ps_props);
                    if let Some(r) = self.cur.as_mut() {
                        merge(&mut r.props, ps);
                    }
                }
            }
            Tag::Response => {
                if let Some(r) = self.cur.take().filter(|r| !r.href.is_empty()) {
                    self.out.push(r);
                }
            }
            _ => {}
        }
    }
}

fn merge(dst: &mut Props, src: Props) {
    dst.collection |= src.collection;
    dst.content_length = src.content_length.or(dst.content_length);
    dst.last_modified = src.last_modified.or(dst.last_modified);
    dst.creation_date = src.creation_date.or(dst.creation_date);
    dst.etag = src.etag.or(dst.etag.take());
    dst.quota_available = src.quota_available.or(dst.quota_available);
    dst.quota_used = src.quota_used.or(dst.quota_used);
}

fn local_name(e: &BytesStart<'_>) -> Vec<u8> {
    e.local_name().as_ref().to_vec()
}

/// 207 응답 본문을 `response` 단위로 나눕니다.
pub(super) fn parse_multistatus(body: &str) -> Result<Vec<DavResponse>, String> {
    let mut reader = NsReader::from_str(body);
    let mut st = State::default();
    loop {
        let (ns, ev) = reader.read_resolved_event().map_err(|e| e.to_string())?;
        match ev {
            Event::Start(e) => {
                let tag = tag_of(&ns, &local_name(&e));
                st.open(tag);
            }
            Event::Empty(e) => {
                let tag = tag_of(&ns, &local_name(&e));
                st.open(tag);
                st.close();
            }
            Event::End(_) => st.close(),
            Event::Text(t) => {
                let s = t.xml10_content().map_err(|e| e.to_string())?;
                st.text.push_str(&s);
            }
            Event::CData(t) => {
                let s = t.decode().map_err(|e| e.to_string())?;
                st.text.push_str(&s);
            }
            Event::GeneralRef(r) => {
                let name = r.decode().map_err(|e| e.to_string())?;
                let resolved = quick_xml::escape::unescape(&format!("&{name};"))
                    .map_err(|e| e.to_string())?
                    .into_owned();
                st.text.push_str(&resolved);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(st.out)
}

/// href(절대 URL 또는 절대 경로, 퍼센트 인코딩)를 디코딩한 경로로 바꿉니다.
/// 상대 href는 `request_dir`(디코딩된 요청 경로) 기준으로 붙입니다.
pub(super) fn href_path(href: &str, request_dir: &str) -> String {
    let raw = if href.starts_with('/') {
        href
    } else if let Some((_, rest)) = href.split_once("://") {
        rest.find('/').map_or("/", |i| &rest[i..])
    } else {
        let joined = format!("{}/{}", request_dir.trim_end_matches('/'), href);
        return percent_decode_str(&joined).decode_utf8_lossy().into_owned();
    };
    // 이름 속 `?`, `#`는 인코딩되어 오므로 날것은 쿼리나 조각입니다.
    let raw = raw.split(['?', '#']).next().unwrap_or(raw);
    percent_decode_str(raw).decode_utf8_lossy().into_owned()
}

/// 디코딩된 경로의 마지막 구성 요소. 후행 `/`는 무시합니다.
pub(super) fn last_segment(path: &str) -> &str {
    path.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

/// 경로 비교용 정규화: 후행 `/` 제거, 연속 `/` 축약
pub(super) fn normalize(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        out.push_str(seg);
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}
