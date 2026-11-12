//! OpenSSH가 Include, Host 패턴과 설정 우선순위를 해석하도록 맡깁니다.
use super::*;

pub(super) struct Resolved {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub identities: Vec<PathBuf>,
    pub known_hosts: Vec<PathBuf>,
}

pub(super) async fn resolve(alias: &str) -> Result<Resolved> {
    // 인자를 셸에 넘기지 않으며 옵션으로 해석될 수 있는 호스트는 거부합니다.
    if alias.is_empty()
        || alias.starts_with('-')
        || !alias
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
    {
        return Err(Error::Config("SSH 별칭을 확인하세요".into()));
    }
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(["-G", "--", alias])
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let output = tokio::time::timeout(Duration::from_secs(10), cmd.output())
        .await
        .map_err(|_| Error::Config("SSH 설정 조회 시간이 초과되었습니다".into()))?
        .map_err(|e| Error::Config(format!("로컬 OpenSSH(ssh -G)를 실행할 수 없습니다: {e}")))?;
    if !output.status.success() {
        return Err(Error::Config(format!(
            "SSH 설정을 읽지 못했습니다: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    parse(&String::from_utf8_lossy(&output.stdout))
}

fn parse(text: &str) -> Result<Resolved> {
    let fields: Vec<_> = text
        .lines()
        .filter_map(|line| line.split_once(' '))
        .collect();
    let get = |name| {
        fields
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.trim())
    };
    for name in ["proxycommand", "proxyjump", "hostkeyalias"] {
        if get(name).is_some_and(|v| v != "none") {
            return Err(Error::Config(format!(
                "SSH 설정의 {name}은 아직 지원하지 않습니다. 직접 연결 또는 앱의 프록시 설정을 사용하세요"
            )));
        }
    }
    let required = |name| {
        get(name)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Error::Config(format!("SSH 설정에 {name}이 없습니다")))
    };
    let host = required("hostname")?.to_owned();
    let user = required("user")?.to_owned();
    let port = required("port")?
        .parse::<u16>()
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| Error::Config("SSH 포트가 올바르지 않습니다".into()))?;
    let home = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(std::env::home_dir)
        .ok_or_else(|| Error::Config("SSH 홈 디렉터리를 찾을 수 없습니다".into()))?;
    let expand = |value: &str| -> Result<PathBuf> {
        let value = value.trim_matches('"');
        let path = if let Some(rest) = value
            .strip_prefix("~/")
            .or_else(|| value.strip_prefix("~\\"))
        {
            home.join(rest)
        } else {
            PathBuf::from(value)
        };
        // -G가 확장하지 않는 토큰을 엉뚱한 파일 경로로 사용하지 않습니다.
        if value.contains('%') || value.contains("${") || !path.is_absolute() {
            return Err(Error::Config(format!(
                "SSH 파일 경로를 절대 경로로 지정하세요: {value}"
            )));
        }
        Ok(path)
    };
    let identities = fields
        .iter()
        .filter(|(k, v)| *k == "identityfile" && v.trim() != "none")
        .map(|(_, v)| expand(v.trim()))
        .collect::<Result<Vec<_>>>()?;
    let paths = split_paths(required("userknownhostsfile")?)?;
    let known_hosts = paths
        .iter()
        .filter(|p| p.as_str() != "none")
        .map(|p| expand(p))
        .collect::<Result<Vec<_>>>()?;
    if known_hosts.is_empty() {
        return Err(Error::Config(
            "SSH 설정에 호스트 키 검증 파일이 필요합니다".into(),
        ));
    }
    Ok(Resolved {
        host,
        port,
        user,
        identities,
        known_hosts,
    })
}

fn split_paths(value: &str) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    let mut path = String::new();
    let mut quoted = false;
    for ch in value.chars() {
        if ch == '"' {
            quoted = !quoted;
        } else if ch.is_whitespace() && !quoted {
            if !path.is_empty() {
                paths.push(std::mem::take(&mut path));
            }
        } else {
            path.push(ch);
        }
    }
    if quoted {
        return Err(Error::Config(
            "SSH 파일 경로의 따옴표가 닫히지 않았습니다".into(),
        ));
    }
    if !path.is_empty() {
        paths.push(path);
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_defaults_and_all_identity_candidates() {
        let r = parse("hostname 192.0.2.1\nuser root\nport 2222\nidentityfile ~/.ssh/id_rsa\nidentityfile ~/.ssh/id_ed25519\nuserknownhostsfile ~/.ssh/known_hosts ~/.ssh/known_hosts2\n").unwrap();
        assert_eq!(
            (r.host.as_str(), r.user.as_str(), r.port),
            ("192.0.2.1", "root", 2222)
        );
        assert_eq!(r.identities.len(), 2);
        assert!(r.identities[1].ends_with(".ssh/id_ed25519"));
        assert!(r.known_hosts[0].ends_with(".ssh/known_hosts"));
    }

    #[test]
    fn quoted_windows_paths_keep_spaces_and_backslashes() {
        assert_eq!(
            split_paths(r#""C:\Users\A B\.ssh\known_hosts" "C:\other file""#).unwrap(),
            vec![r"C:\Users\A B\.ssh\known_hosts", r"C:\other file"]
        );
    }

    #[test]
    fn refuses_unsupported_routes_and_bad_ports() {
        let base =
            "hostname example.com\nuser me\nport 22\nuserknownhostsfile ~/.ssh/known_hosts\n";
        for field in [
            "proxyjump bastion",
            "proxycommand custom-command",
            "hostkeyalias other",
        ] {
            assert!(parse(&format!("{base}{field}\n")).is_err());
        }
        assert!(parse(&base.replace("port 22", "port 0")).is_err());
    }
}
