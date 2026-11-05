//! 로그 출력 직전에 비밀 값과 개인정보를 가립니다.
//!
//! 코드에서 비밀 값을 로그로 남기지 않는 것이 1차 방어이고, 이 계층은 외부 라이브러리 메시지나
//! 실수로 섞인 값을 막는 2차 방어입니다. `tracing_subscriber::fmt().with_writer(Redacting(...))`로 씁니다.

use regex::{Captures, Regex};
use std::borrow::Cow;
use std::io::{self, Write};
use std::sync::LazyLock;
use tracing_subscriber::fmt::MakeWriter;

/// ANSI 색 코드가 키와 값 사이에 끼어도 찾도록 허용하는 구간
const ANSI: &str = r"(?:\x1b\[[0-9;]*m)*";

static RULES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    // 키 앞 경계: 줄 시작, 단어 문자가 아닌 문자, 또는 ANSI 색 코드(`...m` 뒤에 바로 키가 옴).
    // 색 코드 뒤에서는 `\b`가 성립하지 않으므로 다른 규칙도 단어 경계를 쓰지 않습니다.
    let kv = format!(
        r#"(?i)(^|[^a-z0-9_-]|\x1b\[[0-9;]*m)(password|passwd|pwd|pass|passphrase|secret|secret_access_key|client_secret|token|access_token|refresh_token|id_token|api[_-]?key|authorization|proxy-authorization|x-amz-signature|x-amz-credential|x-amz-security-token|signature)({ANSI}["']?\s*[=:]\s*{ANSI}["']?)(?:(?:bearer|basic|digest)\s+)?[^\s"'&,;)}}\]\x1b]+"#
    );
    vec![
        // scheme://user:pass@host 의 자격 증명
        (
            Regex::new(r"(?i)([a-z][a-z0-9+.-]*://)[^/\s:@]+(?::[^/\s@]*)?@").unwrap(),
            "${1}***@",
        ),
        // 키=값, 키: 값 형태의 비밀 값과 서명 쿼리 파라미터(인증 방식 이름 포함)
        (Regex::new(&kv).unwrap(), "${1}${2}${3}***"),
        // 키 없이 남은 Authorization 헤더 값
        (
            Regex::new(r"(?i)(bearer|basic|digest)\s+[A-Za-z0-9._~+/=-]{8,}").unwrap(),
            "$1 ***",
        ),
        // 사용자 프로필 경로의 계정 이름
        (
            Regex::new(r#"(?i)([a-z]:[\\/]+users[\\/]+)[^\\/\s"':]+"#).unwrap(),
            "${1}***",
        ),
    ]
});

/// 한 줄 또는 한 이벤트 문자열을 가립니다. 바뀐 것이 없으면 빌려 온 값을 그대로 돌려줍니다.
pub fn redact(text: &str) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(text);
    for (re, rep) in RULES.iter() {
        if re.is_match(&out) {
            let replaced = re.replace_all(&out, |c: &Captures| {
                let mut s = String::new();
                c.expand(rep, &mut s);
                s
            });
            out = Cow::Owned(replaced.into_owned());
        }
    }
    out
}

/// 출력 대상 앞에 끼워 넣는 마스킹 계층
pub struct Redacting<M>(pub M);

pub struct RedactingWriter<W>(W);

impl<'a, M: MakeWriter<'a>> MakeWriter<'a> for Redacting<M> {
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter(self.0.make_writer())
    }
}

impl<W: Write> Write for RedactingWriter<W> {
    /// fmt 계층은 이벤트 하나를 한 번에 씁니다. 받은 조각 전체를 가린 뒤 내보냅니다.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        self.0.write_all(redact(&text).as_bytes())?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn masks_url_credentials() {
        assert_eq!(
            redact("GET https://alice:hunter2@dav.example.com/a failed"),
            "GET https://***@dav.example.com/a failed"
        );
        assert_eq!(
            redact("socks5://bob@proxy.example.com:1080"),
            "socks5://***@proxy.example.com:1080"
        );
    }

    #[test]
    fn masks_key_values_and_headers() {
        let cases = [
            ("password=hunter2 user=alice", "password=*** user=alice"),
            (
                r#"{"token": "abc.def", "id": 1}"#,
                r#"{"token": "***", "id": 1}"#,
            ),
            (
                "?X-Amz-Credential=AKIA%2F20260101&X-Amz-Signature=deadbeef&x-id=GetObject",
                "?X-Amz-Credential=***&X-Amz-Signature=***&x-id=GetObject",
            ),
            (
                "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.x",
                "Authorization: ***",
            ),
            (
                "header Proxy-Authorization=Basic dXNlcjpwYXNz",
                "header Proxy-Authorization=***",
            ),
            (
                "sent Basic dXNlcjpwYXNzd29yZA== to proxy",
                "sent Basic *** to proxy",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(redact(input), want, "{input}");
        }
    }

    #[test]
    fn masks_profile_user_name() {
        assert_eq!(
            redact(r"cache C:\Users\alice\AppData\Local\ExpanDrv"),
            r"cache C:\Users\***\AppData\Local\ExpanDrv"
        );
        assert_eq!(redact("c:/users/bob/x"), "c:/users/***/x");
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        for s in [
            "passive mode enabled, 3 files",
            "mounted W: (webdav) https://dav.example.com/",
            "업로드 완료 path=/docs/a.txt size=10",
        ] {
            assert!(matches!(redact(s), Cow::Borrowed(_)), "{s}");
        }
    }

    #[test]
    fn handles_ansi_colored_fields() {
        let colored = "\x1b[3mpassword\x1b[0m\x1b[2m=\x1b[0mhunter2 next";
        assert!(!redact(colored).contains("hunter2"));
    }

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn tracing_events_are_redacted() {
        let sink = Sink::default();
        let out = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Redacting(move || sink.clone()))
            .with_ansi(true)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let url = "https://alice:hunter2@dav.example.com/";
            let password = "hunter2";
            tracing::warn!(%url, password, "연결 실패");
        });
        let text = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        assert!(text.contains("연결 실패"), "{text}");
        assert!(
            !text.contains("hunter2") && !text.contains("alice"),
            "{text}"
        );
    }
}
