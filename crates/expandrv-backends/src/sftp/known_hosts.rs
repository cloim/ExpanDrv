//! OpenSSH `known_hosts` 검사. 자동 등록(TOFU)은 하지 않습니다.

use data_encoding::BASE64;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::ssh_key::PublicKey;
use sha1::Sha1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyCheck {
    Trusted,
    /// 이 호스트 기록이 없습니다.
    Unknown,
    /// 이 호스트 기록은 있지만 키가 다릅니다(중간자 공격 가능성).
    Mismatch {
        line: usize,
    },
    /// `@revoked`로 폐기된 키입니다.
    Revoked {
        line: usize,
    },
}

/// `known_hosts` 본문에서 `host:port`의 서버 키를 확인합니다.
pub fn check(text: &str, host: &str, port: u16, key: &PublicKey) -> HostKeyCheck {
    let target = host_pattern(host, port);
    let mut mismatch = None;
    let mut trusted = false;
    for (idx, raw) in text.lines().enumerate() {
        let line = idx + 1;
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') {
            continue;
        }
        let mut fields = raw.split_whitespace();
        let Some(mut first) = fields.next() else {
            continue;
        };
        let mut marker = None;
        if first.starts_with('@') {
            marker = Some(first);
            match fields.next() {
                Some(f) => first = f,
                None => continue,
            }
        }
        let (Some(algo), Some(b64)) = (fields.next(), fields.next()) else {
            continue;
        };
        if !hosts_match(first, &target) {
            continue;
        }
        let Ok(recorded) = PublicKey::from_openssh(&format!("{algo} {b64}")) else {
            continue;
        };
        let same = recorded.key_data() == key.key_data();
        match marker {
            Some("@revoked") if same => return HostKeyCheck::Revoked { line },
            Some(_) => {}
            None if same => trusted = true,
            None => {
                mismatch.get_or_insert(line);
            }
        }
    }
    match (trusted, mismatch) {
        (true, _) => HostKeyCheck::Trusted,
        (false, Some(line)) => HostKeyCheck::Mismatch { line },
        (false, None) => HostKeyCheck::Unknown,
    }
}

/// 22번 포트는 `host`, 그 외는 `[host]:port` 형태로 기록됩니다.
fn host_pattern(host: &str, port: u16) -> String {
    let host = host.to_ascii_lowercase();
    if port == 22 {
        host
    } else {
        format!("[{host}]:{port}")
    }
}

/// 쉼표로 나뉜 패턴 목록. `!` 부정 패턴이 맞으면 전체가 불일치입니다.
fn hosts_match(patterns: &str, target: &str) -> bool {
    let mut matched = false;
    for pat in patterns.split(',') {
        if let Some(hashed) = pat.strip_prefix("|1|") {
            matched |= hashed_match(hashed, target);
        } else if let Some(neg) = pat.strip_prefix('!') {
            if wildcard_match(&neg.to_ascii_lowercase(), target) {
                return false;
            }
        } else if wildcard_match(&pat.to_ascii_lowercase(), target) {
            matched = true;
        }
    }
    matched
}

/// `|1|salt|hash` 형식(HMAC-SHA1) 해시 호스트
fn hashed_match(rest: &str, target: &str) -> bool {
    let Some((salt, hash)) = rest.split_once('|') else {
        return false;
    };
    let (Ok(salt), Ok(hash)) = (
        BASE64.decode(salt.as_bytes()),
        BASE64.decode(hash.as_bytes()),
    ) else {
        return false;
    };
    let Ok(mac) = Hmac::<Sha1>::new_from_slice(&salt) else {
        return false;
    };
    mac.chain_update(target.as_bytes())
        .verify_slice(&hash)
        .is_ok()
}

/// `*`와 `?` 와일드카드 일치
fn wildcard_match(pat: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pat.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAILixT8uYxgANE7LoChCHYdKLS0BiU3QETdh9dT0jko0+";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKqg8nBCASnj7gvwPwyFfFUXBOZa2o3UHDFyWW7asKAb";

    fn key(s: &str) -> PublicKey {
        PublicKey::from_openssh(s).unwrap()
    }

    fn hashed(host: &str) -> String {
        let salt = b"0123456789abcdefghij";
        let mac = Hmac::<Sha1>::new_from_slice(salt)
            .unwrap()
            .chain_update(host.as_bytes())
            .finalize()
            .into_bytes();
        format!("|1|{}|{}", BASE64.encode(salt), BASE64.encode(&mac))
    }

    #[test]
    fn plain_and_bracketed_ports() {
        let text = format!("# c\nexample.com {KEY_A}\n[127.0.0.1]:2222 {KEY_A} comment\n");
        assert_eq!(
            check(&text, "Example.COM", 22, &key(KEY_A)),
            HostKeyCheck::Trusted
        );
        assert_eq!(
            check(&text, "127.0.0.1", 2222, &key(KEY_A)),
            HostKeyCheck::Trusted
        );
        assert_eq!(
            check(&text, "127.0.0.1", 22, &key(KEY_A)),
            HostKeyCheck::Unknown
        );
        assert_eq!(
            check(&text, "example.com", 22, &key(KEY_B)),
            HostKeyCheck::Mismatch { line: 2 }
        );
    }

    #[test]
    fn hashed_wildcard_negation_and_revoked() {
        let text = format!(
            "{} {KEY_A}\n*.lan,!bad.lan {KEY_A}\n@revoked old.lan {KEY_A}\nold.lan {KEY_A}\n",
            hashed("[sftp.example]:2022")
        );
        assert_eq!(
            check(&text, "sftp.example", 2022, &key(KEY_A)),
            HostKeyCheck::Trusted
        );
        assert_eq!(
            check(&text, "sftp.example", 22, &key(KEY_A)),
            HostKeyCheck::Unknown
        );
        assert_eq!(
            check(&text, "nas.lan", 22, &key(KEY_A)),
            HostKeyCheck::Trusted
        );
        assert_eq!(
            check(&text, "bad.lan", 22, &key(KEY_A)),
            HostKeyCheck::Unknown
        );
        assert_eq!(
            check(&text, "old.lan", 22, &key(KEY_A)),
            HostKeyCheck::Revoked { line: 3 }
        );
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("*.example.com", "a.b.example.com"));
        assert!(wildcard_match("host?", "host1"));
        assert!(!wildcard_match("host?", "host12"));
        assert!(wildcard_match("*", ""));
        assert!(!wildcard_match("a*c", "abd"));
    }
}
