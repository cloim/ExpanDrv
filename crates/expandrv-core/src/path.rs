use crate::{Error, Result};
use std::fmt;

/// 정규화된 원격 경로. 루트는 `/`, 그 외는 `/a/b` 형태이며 후행 구분자가 없습니다.
///
/// `.`, `..`, 빈 구성 요소, 제어 문자와 Windows 금지 문자를 거부하므로
/// 백엔드는 이 값을 그대로 원격 경로로 이어 붙여도 루트 밖으로 벗어나지 않습니다.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RemotePath(String);

/// NTFS와 같은 구성 요소 길이 상한(UTF-16 코드 단위)
const MAX_COMPONENT_UTF16: usize = 255;

impl RemotePath {
    pub fn root() -> Self {
        Self("/".to_owned())
    }

    /// `/`와 `\` 구분자를 모두 받아 정규화합니다. WinFsp 경로(`\dir\file`)도 이 함수로 변환합니다.
    pub fn parse(raw: &str) -> Result<Self> {
        let mut out = String::with_capacity(raw.len() + 1);
        for part in raw.split(['/', '\\']).filter(|p| !p.is_empty()) {
            validate_component(part)?;
            out.push('/');
            out.push_str(part);
        }
        if out.is_empty() {
            out.push('/');
        }
        Ok(Self(out))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 선행 `/`를 뺀 상대 경로. 루트는 빈 문자열입니다.
    pub fn relative(&self) -> &str {
        &self.0[1..]
    }

    pub fn is_root(&self) -> bool {
        self.0 == "/"
    }

    pub fn name(&self) -> Option<&str> {
        if self.is_root() {
            None
        } else {
            self.0.rsplit('/').next()
        }
    }

    pub fn parent(&self) -> Option<RemotePath> {
        if self.is_root() {
            return None;
        }
        let idx = self.0.rfind('/').unwrap_or(0);
        Some(if idx == 0 {
            Self::root()
        } else {
            Self(self.0[..idx].to_owned())
        })
    }

    pub fn join(&self, name: &str) -> Result<RemotePath> {
        validate_component(name)?;
        let mut s = self.0.clone();
        if !self.is_root() {
            s.push('/');
        }
        s.push_str(name);
        Ok(Self(s))
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.relative().split('/').filter(|p| !p.is_empty())
    }

    /// `self`가 `ancestor`와 같거나 그 하위 경로인지 확인합니다.
    pub fn starts_with(&self, ancestor: &RemotePath) -> bool {
        ancestor.is_root()
            || self.0 == ancestor.0
            || (self.0.starts_with(&ancestor.0) && self.0.as_bytes()[ancestor.0.len()] == b'/')
    }

    /// `old` 접두 경로를 `new`로 바꿉니다. 이름 변경 후 하위 항목 경로 갱신에 사용합니다.
    pub fn rebase(&self, old: &RemotePath, new: &RemotePath) -> Option<RemotePath> {
        if !self.starts_with(old) {
            return None;
        }
        let rest = if old.is_root() {
            self.relative()
        } else {
            self.0[old.0.len()..].trim_start_matches('/')
        };
        let mut s = new.0.clone();
        if !rest.is_empty() {
            if !new.is_root() {
                s.push('/');
            }
            s.push_str(rest);
        }
        Some(Self(s))
    }
}

fn validate_component(part: &str) -> Result<()> {
    let invalid = |why: &str| Err(Error::InvalidPath(format!("{part:?}: {why}")));
    if part == "." || part == ".." {
        return invalid("상대 경로 구성 요소");
    }
    if part.encode_utf16().count() > MAX_COMPONENT_UTF16 {
        return invalid("이름이 너무 김");
    }
    if part
        .chars()
        .any(|c| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
    {
        return invalid("허용되지 않는 문자");
    }
    Ok(())
}

impl fmt::Display for RemotePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RemotePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_normalizes_separators() {
        assert_eq!(RemotePath::parse("\\a\\b\\").unwrap().as_str(), "/a/b");
        assert_eq!(RemotePath::parse("a//b").unwrap().as_str(), "/a/b");
        assert!(RemotePath::parse("\\").unwrap().is_root());
        assert!(RemotePath::parse("").unwrap().is_root());
    }

    #[test]
    fn parse_rejects_traversal_and_reserved_chars() {
        for bad in ["/a/../b", "/./a", "/a:stream", "/a*b", "/a\u{0}b"] {
            assert!(RemotePath::parse(bad).is_err(), "{bad}");
        }
        assert!(RemotePath::root().join("..").is_err());
        assert!(RemotePath::parse(&"x".repeat(256)).is_err());
    }

    #[test]
    fn parent_name_join() {
        let p = RemotePath::parse("/a/b/c.txt").unwrap();
        assert_eq!(p.name(), Some("c.txt"));
        assert_eq!(p.parent().unwrap().as_str(), "/a/b");
        assert_eq!(
            RemotePath::parse("/a").unwrap().parent(),
            Some(RemotePath::root())
        );
        assert_eq!(RemotePath::root().join("x").unwrap().as_str(), "/x");
        assert_eq!(p.components().collect::<Vec<_>>(), ["a", "b", "c.txt"]);
    }

    #[test]
    fn starts_with_respects_component_boundary() {
        let a = RemotePath::parse("/ab").unwrap();
        let b = RemotePath::parse("/a").unwrap();
        assert!(!a.starts_with(&b));
        assert!(RemotePath::parse("/a/x").unwrap().starts_with(&b));
        assert!(a.starts_with(&RemotePath::root()));
    }

    #[test]
    fn rebase_moves_subtree() {
        let old = RemotePath::parse("/a").unwrap();
        let new = RemotePath::parse("/z/y").unwrap();
        let p = RemotePath::parse("/a/b/c").unwrap();
        assert_eq!(p.rebase(&old, &new).unwrap().as_str(), "/z/y/b/c");
        assert_eq!(old.rebase(&old, &new).unwrap(), new);
        assert!(
            RemotePath::parse("/ab")
                .unwrap()
                .rebase(&old, &new)
                .is_none()
        );
    }
}
