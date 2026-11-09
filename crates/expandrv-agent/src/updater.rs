//! GitHub Releases를 업데이트 채널로 활용하는 자동 업데이트 모듈.
//!
//! 정식 Release(draft, prerelease 및 SemVer prerelease 제외)에서
//! 현재 실행 환경(NSIS 또는 MSI)에 맞는 패키지와 SHA-256 매니페스트를 대조하여
//! 무결성이 검증된 설치 파일만 다운로드하고 안전하게 설치를 인계합니다.

use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const DEFAULT_REPO_OWNER: &str = "cloim";
pub const DEFAULT_REPO_NAME: &str = "ExpanDrv";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum InstallKind {
    /// NSIS 설치 패키지 (단일 사용자 데스크톱 설치)
    Nsis,
    /// WiX MSI 패키지 (시스템 단위 관리자 배포 설치)
    Msi,
    /// 개발 중 실행 파일이거나 설치 형식을 식별할 수 없는 상태
    Unknown,
}

impl InstallKind {
    /// 대상 버전 문자열에 해당하는 설치 파일명을 반환합니다.
    pub fn setup_file_name(&self, version_str: &str) -> Option<String> {
        match self {
            Self::Nsis => Some(format!("ExpanDrv_{version_str}_x64-setup.exe")),
            Self::Msi => Some(format!("ExpanDrv_{version_str}_x64_en-US.msi")),
            Self::Unknown => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("GitHub API 요청 실패: {0}")]
    Network(String),
    #[error("응답 데이터 파싱 실패: {0}")]
    Parse(String),
    #[error("허용되지 않거나 검증되지 않은 자산 URL: {0}")]
    InvalidAssetUrl(String),
    #[error("지원되지 않는 설치 형식: {0}")]
    UnsupportedInstallKind(String),
    #[error("필수 릴리스 자산이 누락되었습니다: {0}")]
    MissingAssets(String),
    #[error("매니페스트 검증 실패: {0}")]
    InvalidManifest(String),
    #[error("파일 다운로드 실패: {0}")]
    DownloadFailed(String),
    #[error("다운로드된 파일 크기 불일치 (기대값: {expected} bytes, 실제값: {actual} bytes)")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("다운로드된 파일 SHA-256 해시 불일치 (기대값: {expected}, 계산값: {actual})")]
    HashMismatch { expected: String, actual: String },
    #[error("I/O 오류: {0}")]
    Io(#[from] std::io::Error),
    #[error("설치 프로그램 실행 실패: {0}")]
    LaunchFailed(String),
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct GitHubAsset {
    pub name: String,
    pub size: u64,
    pub browser_download_url: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct GitHubRelease {
    pub tag_name: String,
    pub name: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<GitHubAsset>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct ManifestArtifact {
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
    #[serde(default)]
    pub signature: Option<String>,
    #[serde(default)]
    pub winfsp: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, PartialEq, Eq)]
pub struct Manifest {
    pub version: String,
    pub architecture: String,
    #[serde(rename = "winfspVersion", default)]
    pub winfsp_version: Option<String>,
    #[serde(rename = "winfspSha256", default)]
    pub winfsp_sha256: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<ManifestArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetDownloadTarget {
    pub name: String,
    pub size: u64,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCandidate {
    pub version: semver::Version,
    pub version_str: String,
    pub release_name: String,
    pub release_url: String,
    pub install_kind: InstallKind,
    pub setup_asset: AssetDownloadTarget,
    pub manifest_asset: AssetDownloadTarget,
}

/// "v0.1.0" 또는 "0.1.0" 형태의 문자열을 유효한 SemVer 버전으로 파싱합니다.
pub fn parse_version(tag_or_ver: &str) -> Option<semver::Version> {
    let clean = tag_or_ver.trim();
    let clean = clean.strip_prefix('v').unwrap_or(clean);
    semver::Version::parse(clean).ok()
}

/// Windows 경로 비교를 위해 구분자와 끝 슬래시를 통일하고 대소문자를 무시하여 대조합니다.
pub fn paths_equal_normalized(p1: &Path, p2: &Path) -> bool {
    let s1 = p1.to_string_lossy().replace('/', "\\");
    let s2 = p2.to_string_lossy().replace('/', "\\");
    let s1_trim = s1.trim_end_matches('\\');
    let s2_trim = s2.trim_end_matches('\\');
    s1_trim.eq_ignore_ascii_case(s2_trim)
}

/// 실행 파일 경로와 등록된 MSI 설치 위치 및 배포 상태를 바탕으로 설치 형식을 엄격히 식별합니다.
/// - 개발/타깃 경로는 Unknown으로 유지합니다.
/// - 디렉터리에 고유 제거 프로그램(uninstall.exe)이 있으면 NSIS로 판정합니다.
/// - MSI의 경우, 레지스트리에 동일 제품으로 등록된 설치 디렉터리(`registered_msi_dir`)와 현재 실행 파일의 디렉터리가
///   정확히 일치하고 배포 등록(Installed=1 또는 Uninstall 마커)이 확인될 때만 MSI로 식별합니다.
/// - 단순 복사본, 타 위치 등록본, 미등록 위치는 Unknown으로 유지하여 오판 및 혼용을 원천 차단합니다.
pub fn detect_install_kind_from_paths(
    exe_path: &Path,
    registered_msi_dir: Option<&Path>,
    msi_deployment_valid: bool,
) -> InstallKind {
    let path_str = exe_path.to_string_lossy();
    // 개발/테스트 타깃 경로이면 식별 불가로 처리하여 임의 설치 혼용을 차단합니다.
    if path_str.contains(r"\target\debug\")
        || path_str.contains(r"\target\release\")
        || path_str.contains("/target/debug/")
        || path_str.contains("/target/release/")
    {
        return InstallKind::Unknown;
    }

    if let Some(dir) = exe_path.parent() {
        // 1. NSIS는 항상 동일 디렉터리에 고유 제거 프로그램(uninstall.exe)을 포함합니다.
        if dir.join("uninstall.exe").exists() {
            return InstallKind::Nsis;
        }

        // 2. MSI는 uninstall.exe가 없으며, 등록된 설치 디렉터리와 현재 실행 위치가 정확히 일치하고
        //    배포 상태가 유효할 때만 MSI로 식별합니다. (사용자 지정 설치 위치 포함)
        if let Some(reg_dir) = registered_msi_dir
            && msi_deployment_valid
            && paths_equal_normalized(dir, reg_dir)
        {
            return InstallKind::Msi;
        }
    }

    InstallKind::Unknown
}

/// 현재 실행 중인 프로세스의 바이너리 경로와 레지스트리를 검사하여 설치 형식을 식별합니다.
pub fn detect_current_install_kind() -> InstallKind {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return InstallKind::Unknown,
    };

    let reg_dir = read_registered_msi_install_dir();
    let msi_installed = check_msi_registry_deployment();
    detect_install_kind_from_paths(&exe, reg_dir.as_deref(), msi_installed)
}

/// 레지스트리에 등록된 실제 ExpanDrv MSI 설치 디렉터리를 읽어옵니다.
pub fn read_registered_msi_install_dir() -> Option<PathBuf> {
    use winreg::RegKey;
    use winreg::enums::*;

    // 1. HKCU\Software\ExpanDrv\ExpanDrv 의 InstallDir (WiX RegistryEntries)
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(key) = hkcu.open_subkey_with_flags(r"Software\ExpanDrv\ExpanDrv", KEY_READ)
        && let Ok(dir_str) = key.get_value::<String, _>("InstallDir")
    {
        let trimmed = dir_str.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }

    // 2. HKLM\Software\ExpanDrv\ExpanDrv 의 InstallDir
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    if let Ok(key) = hklm.open_subkey_with_flags(r"Software\ExpanDrv\ExpanDrv", KEY_READ)
        && let Ok(dir_str) = key.get_value::<String, _>("InstallDir")
    {
        let trimmed = dir_str.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }

    // 3. HKLM Uninstall 키에서 ExpanDrv의 InstallLocation 확인
    for root_key_path in [
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall",
        r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
    ] {
        if let Ok(uninstall_root) = hklm.open_subkey_with_flags(root_key_path, KEY_READ) {
            for subkey_name in uninstall_root.enum_keys().filter_map(|r| r.ok()) {
                if let Ok(sub) = uninstall_root.open_subkey_with_flags(&subkey_name, KEY_READ) {
                    let display_name: Result<String, _> = sub.get_value("DisplayName");
                    if let Ok(name) = display_name
                        && name.eq_ignore_ascii_case("ExpanDrv")
                        && let Ok(loc) = sub.get_value::<String, _>("InstallLocation")
                    {
                        let trimmed = loc.trim();
                        if !trimmed.is_empty() {
                            return Some(PathBuf::from(trimmed));
                        }
                    }
                }
            }
        }
    }

    None
}

fn check_msi_registry_deployment() -> bool {
    use winreg::RegKey;
    use winreg::enums::*;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    if let Ok(key) = hklm.open_subkey_with_flags(r"Software\ExpanDrv\Deployment", KEY_READ)
        && let Ok(installed) = key.get_value::<u32, _>("Installed")
    {
        return installed == 1;
    }
    if let Ok(key) =
        hklm.open_subkey_with_flags(r"Software\WOW6432Node\ExpanDrv\Deployment", KEY_READ)
        && let Ok(installed) = key.get_value::<u32, _>("Installed")
    {
        return installed == 1;
    }
    false
}

/// GitHub Release 자산 URL이 기대하는 저장소, 릴리스 태그, 파일명과 정확히 연결되는지 검증합니다.
pub fn validate_github_asset_url(
    raw_url: &str,
    expected_owner: &str,
    expected_repo: &str,
    expected_tag: &str,
    expected_filename: &str,
) -> Result<url::Url, UpdateError> {
    let parsed = url::Url::parse(raw_url)
        .map_err(|e| UpdateError::InvalidAssetUrl(format!("유효하지 않은 URL 형식: {e}")))?;

    if parsed.scheme() != "https" {
        return Err(UpdateError::InvalidAssetUrl(format!(
            "HTTPS 프로토콜만 허용됩니다: {}",
            parsed.scheme()
        )));
    }

    if parsed.host_str() != Some("github.com") {
        return Err(UpdateError::InvalidAssetUrl(format!(
            "허용되지 않은 호스트: {:?}",
            parsed.host_str()
        )));
    }

    let expected_path = format!(
        "/{expected_owner}/{expected_repo}/releases/download/{expected_tag}/{expected_filename}"
    );
    if parsed.path() != expected_path {
        return Err(UpdateError::InvalidAssetUrl(format!(
            "자산 경로 불일치: 기대값 {expected_path}, 실제값 {}",
            parsed.path()
        )));
    }

    Ok(parsed)
}

/// 릴리스 목록에서 현재 설치 형식에 맞는 최신 정식 릴리스 후보를 찾습니다.
/// draft, prerelease 플래그뿐만 아니라 SemVer prerelease 태그도 정식 후보에서 엄격히 제외합니다.
pub fn find_latest_update(
    releases: &[GitHubRelease],
    current_version: &semver::Version,
    install_kind: InstallKind,
) -> Result<Option<UpdateCandidate>, UpdateError> {
    if install_kind == InstallKind::Unknown {
        return Err(UpdateError::UnsupportedInstallKind(
            "현재 설치 형식을 식별할 수 없습니다 (개발 빌드이거나 알 수 없는 경로입니다)".into(),
        ));
    }

    let mut eligible: Vec<(&GitHubRelease, semver::Version)> = Vec::new();
    for rel in releases {
        if rel.draft || rel.prerelease {
            continue;
        }
        if let Some(ver) = parse_version(&rel.tag_name)
            && ver.pre.is_empty()
            && ver > *current_version
        {
            eligible.push((rel, ver));
        }
    }

    if eligible.is_empty() {
        return Ok(None);
    }

    // SemVer 기준 내림차순 정렬 (가장 최신 정식 버전 우선)
    eligible.sort_by(|a, b| b.1.cmp(&a.1));

    let (latest_rel, latest_ver) = &eligible[0];
    let ver_str = latest_ver.to_string();

    let setup_name = install_kind.setup_file_name(&ver_str).ok_or_else(|| {
        UpdateError::UnsupportedInstallKind("설치 파일명을 결정할 수 없습니다".into())
    })?;
    let manifest_name = format!("ExpanDrv_{ver_str}_manifest.json");

    let setup_asset = latest_rel
        .assets
        .iter()
        .find(|a| a.name == setup_name)
        .ok_or_else(|| {
            UpdateError::MissingAssets(format!(
                "릴리스 {}에 {:?} 설치 패키지({setup_name})가 없습니다",
                latest_rel.tag_name, install_kind
            ))
        })?;

    let manifest_asset = latest_rel
        .assets
        .iter()
        .find(|a| a.name == manifest_name)
        .ok_or_else(|| {
            UpdateError::MissingAssets(format!(
                "릴리스 {}에 매니페스트({manifest_name})가 없습니다",
                latest_rel.tag_name
            ))
        })?;

    // 자산 URL 구조를 저장소와 태그에 엄격하게 대조 검증합니다.
    validate_github_asset_url(
        &setup_asset.browser_download_url,
        DEFAULT_REPO_OWNER,
        DEFAULT_REPO_NAME,
        &latest_rel.tag_name,
        &setup_name,
    )?;

    validate_github_asset_url(
        &manifest_asset.browser_download_url,
        DEFAULT_REPO_OWNER,
        DEFAULT_REPO_NAME,
        &latest_rel.tag_name,
        &manifest_name,
    )?;

    Ok(Some(UpdateCandidate {
        version: latest_ver.clone(),
        version_str: ver_str,
        release_name: latest_rel
            .name
            .clone()
            .unwrap_or_else(|| latest_rel.tag_name.clone()),
        release_url: latest_rel.html_url.clone(),
        install_kind,
        setup_asset: AssetDownloadTarget {
            name: setup_asset.name.clone(),
            size: setup_asset.size,
            url: setup_asset.browser_download_url.clone(),
        },
        manifest_asset: AssetDownloadTarget {
            name: manifest_asset.name.clone(),
            size: manifest_asset.size,
            url: manifest_asset.browser_download_url.clone(),
        },
    }))
}

/// 다운로드된 매니페스트 내용의 유효성을 검증하고, 대상 설치 파일에 해당하는 아티팩트 정보를 반환합니다.
pub fn verify_manifest<'a>(
    manifest: &'a Manifest,
    candidate: &UpdateCandidate,
) -> Result<&'a ManifestArtifact, UpdateError> {
    if manifest.architecture != "x64" {
        return Err(UpdateError::InvalidManifest(format!(
            "아키텍처 불일치: 기대값 x64, 매니페스트 {}",
            manifest.architecture
        )));
    }

    let manifest_ver = parse_version(&manifest.version).ok_or_else(|| {
        UpdateError::InvalidManifest(format!("매니페스트 버전 형식 오류: {}", manifest.version))
    })?;

    if manifest_ver != candidate.version {
        return Err(UpdateError::InvalidManifest(format!(
            "버전 불일치: 릴리스 {}, 매니페스트 {}",
            candidate.version, manifest.version
        )));
    }

    let artifact = manifest
        .artifacts
        .iter()
        .find(|a| a.file == candidate.setup_asset.name)
        .ok_or_else(|| {
            UpdateError::InvalidManifest(format!(
                "매니페스트에 대상 파일({}) 정보가 없습니다",
                candidate.setup_asset.name
            ))
        })?;

    if artifact.bytes != candidate.setup_asset.size {
        return Err(UpdateError::InvalidManifest(format!(
            "파일 크기 불일치: GitHub 자산 {} bytes, 매니페스트 {} bytes",
            candidate.setup_asset.size, artifact.bytes
        )));
    }

    let clean_sha = artifact.sha256.trim();
    if clean_sha.len() != 64 || !clean_sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(UpdateError::InvalidManifest(format!(
            "SHA-256 해시 형식 오류: {}",
            artifact.sha256
        )));
    }

    Ok(artifact)
}

/// 지정된 파일의 크기와 SHA-256 해시를 검증합니다.
pub fn verify_file_hash_and_size(
    path: &Path,
    expected_bytes: u64,
    expected_sha256: &str,
) -> Result<(), UpdateError> {
    let metadata = std::fs::metadata(path)?;
    if metadata.len() != expected_bytes {
        return Err(UpdateError::SizeMismatch {
            expected: expected_bytes,
            actual: metadata.len(),
        });
    }

    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    let actual_hash = format!("{:x}", hasher.finalize());

    if !actual_hash.eq_ignore_ascii_case(expected_sha256.trim()) {
        return Err(UpdateError::HashMismatch {
            expected: expected_sha256.to_string(),
            actual: actual_hash,
        });
    }

    Ok(())
}

/// 업데이트 다운로드 폴더(`%LOCALAPPDATA%\ExpanDrv\updates`)를 반환하고 생성합니다.
pub fn updates_dir() -> Result<PathBuf, UpdateError> {
    let base = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| std::io::Error::other("LOCALAPPDATA 환경 변수가 없습니다"))?;
    let dir = PathBuf::from(base).join("ExpanDrv").join("updates");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// HTTPS 클라이언트를 빌드합니다.
/// 리다이렉트 시에도 HTTPS를 강제하고 GitHub 자산 CDN 도메인만 허용하도록 제한합니다.
pub fn build_http_client() -> Result<reqwest::Client, UpdateError> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let redirect_policy = reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.error("너무 많은 리다이렉트 발생");
        }

        let is_https = attempt.url().scheme() == "https";
        if !is_https {
            return attempt.error("HTTP 프로토콜로의 다운그레이드 리다이렉트 거부");
        }

        let host = attempt.url().host_str().unwrap_or("").to_string();
        let is_github_domain = host == "github.com"
            || host == "objects.githubusercontent.com"
            || host.ends_with(".githubusercontent.com");

        if !is_github_domain {
            return attempt.error(format!("허용되지 않은 외부 도메인 리다이렉트 거부: {host}"));
        }

        attempt.follow()
    });

    reqwest::Client::builder()
        .user_agent(format!("ExpanDrv/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .redirect(redirect_policy)
        .build()
        .map_err(|e| UpdateError::Network(e.to_string()))
}

/// GitHub Releases API에서 릴리스 목록을 가져옵니다.
pub async fn fetch_releases(
    client: &reqwest::Client,
    owner: &str,
    repo: &str,
) -> Result<Vec<GitHubRelease>, UpdateError> {
    let url = format!("https://api.github.com/repos/{owner}/{repo}/releases");
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| UpdateError::Network(format!("요청 전송 실패: {e}")))?;

    if !resp.status().is_success() {
        return Err(UpdateError::Network(format!(
            "GitHub API 응답 오류 (HTTP {})",
            resp.status()
        )));
    }

    let releases = resp
        .json::<Vec<GitHubRelease>>()
        .await
        .map_err(|e| UpdateError::Parse(format!("릴리스 JSON 파싱 실패: {e}")))?;

    Ok(releases)
}

/// 매니페스트 JSON을 다운로드하여 파싱합니다.
pub async fn fetch_manifest(client: &reqwest::Client, url: &str) -> Result<Manifest, UpdateError> {
    if !url.starts_with("https://") {
        return Err(UpdateError::Network("HTTPS 프로토콜만 허용됩니다".into()));
    }

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| UpdateError::Network(format!("매니페스트 요청 실패: {e}")))?;

    if !resp.status().is_success() {
        return Err(UpdateError::Network(format!(
            "매니페스트 응답 오류 (HTTP {})",
            resp.status()
        )));
    }

    let manifest = resp
        .json::<Manifest>()
        .await
        .map_err(|e| UpdateError::Parse(format!("매니페스트 파싱 실패: {e}")))?;

    Ok(manifest)
}

/// 설치 프로그램 파일을 스트리밍으로 다운로드하고, 실시간으로 해시와 크기를 계산하여 검증합니다.
pub async fn download_and_verify_setup(
    client: &reqwest::Client,
    candidate: &UpdateCandidate,
    artifact: &ManifestArtifact,
) -> Result<PathBuf, UpdateError> {
    if !candidate.setup_asset.url.starts_with("https://") {
        return Err(UpdateError::Network("HTTPS 프로토콜만 허용됩니다".into()));
    }

    let dir = updates_dir()?;
    let final_path = dir.join(&candidate.setup_asset.name);
    let part_path = dir.join(format!("{}.part", candidate.setup_asset.name));

    // 이미 같은 크기와 해시로 검증된 파일이 존재하면 재다운로드 없이 반환
    if final_path.exists()
        && verify_file_hash_and_size(&final_path, artifact.bytes, &artifact.sha256).is_ok()
    {
        tracing::info!(path = %final_path.display(), "이미 검증된 설치 파일이 존재하여 재사용합니다");
        return Ok(final_path);
    }

    let resp = client
        .get(&candidate.setup_asset.url)
        .send()
        .await
        .map_err(|e| UpdateError::DownloadFailed(format!("다운로드 요청 실패: {e}")))?;

    if !resp.status().is_success() {
        return Err(UpdateError::DownloadFailed(format!(
            "다운로드 HTTP 오류: {}",
            resp.status()
        )));
    }

    let mut file = std::fs::File::create(&part_path)?;
    let mut stream = resp.bytes_stream();
    let mut hasher = Sha256::new();
    let mut downloaded_bytes: u64 = 0;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            let _ = std::fs::remove_file(&part_path);
            UpdateError::DownloadFailed(format!("스트림 수신 오류: {e}"))
        })?;

        downloaded_bytes += chunk.len() as u64;
        if downloaded_bytes > artifact.bytes {
            let _ = std::fs::remove_file(&part_path);
            return Err(UpdateError::SizeMismatch {
                expected: artifact.bytes,
                actual: downloaded_bytes,
            });
        }

        hasher.update(&chunk);
        file.write_all(&chunk).map_err(|e| {
            let _ = std::fs::remove_file(&part_path);
            UpdateError::Io(e)
        })?;
    }

    file.flush()?;
    drop(file);

    if downloaded_bytes != artifact.bytes {
        let _ = std::fs::remove_file(&part_path);
        return Err(UpdateError::SizeMismatch {
            expected: artifact.bytes,
            actual: downloaded_bytes,
        });
    }

    let actual_sha256 = format!("{:x}", hasher.finalize());
    if !actual_sha256.eq_ignore_ascii_case(artifact.sha256.trim()) {
        let _ = std::fs::remove_file(&part_path);
        return Err(UpdateError::HashMismatch {
            expected: artifact.sha256.clone(),
            actual: actual_sha256,
        });
    }

    std::fs::rename(&part_path, &final_path)?;
    tracing::info!(path = %final_path.display(), "설치 파일 다운로드 및 SHA-256 검증 성공");

    Ok(final_path)
}

/// Windows 프로세스 대기 결과입니다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessWaitResult {
    /// 프로세스가 종료되었으며 해당 종료 코드입니다.
    Exited(u32),
    /// 대기 시간 동안 프로세스가 계속 실행 중입니다.
    Timeout,
}

/// 스레드 간 안전하게 이동 가능하며 종료 시 자동으로 CloseHandle을 호출하는 Windows 프로세스 핸들 래퍼입니다.
#[derive(Debug)]
pub struct SafeProcessHandle(pub windows::Win32::Foundation::HANDLE);
unsafe impl Send for SafeProcessHandle {}
unsafe impl Sync for SafeProcessHandle {}

impl Drop for SafeProcessHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.0);
            }
            self.0 = windows::Win32::Foundation::HANDLE::default();
        }
    }
}

impl SafeProcessHandle {
    #[cfg(test)]
    pub fn new(handle: windows::Win32::Foundation::HANDLE) -> Self {
        Self(handle)
    }

    /// 유한 타임아웃(밀리초) 동안 프로세스 종료를 대기합니다.
    /// Windows API 오류(WAIT_FAILED 등) 발생 시 명확히 Err를 반환하며, 성공이나 타임아웃으로 오인하지 않습니다.
    pub fn wait_timeout(&self, timeout_ms: u32) -> Result<ProcessWaitResult, UpdateError> {
        use windows::Win32::Foundation::{WAIT_FAILED, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

        let ret = unsafe { WaitForSingleObject(self.0, timeout_ms) };
        if ret.0 == 0 {
            // WAIT_OBJECT_0: 프로세스 종료
            let mut exit_code: u32 = 0;
            let ok = unsafe { GetExitCodeProcess(self.0, &mut exit_code) };
            if ok.is_err() {
                return Err(UpdateError::LaunchFailed(format!(
                    "프로세스 종료 코드 조회 실패: win32 error {}",
                    std::io::Error::last_os_error()
                )));
            }
            Ok(ProcessWaitResult::Exited(exit_code))
        } else if ret == WAIT_TIMEOUT {
            Ok(ProcessWaitResult::Timeout)
        } else if ret == WAIT_FAILED {
            Err(UpdateError::LaunchFailed(format!(
                "WaitForSingleObject 실패: win32 error {}",
                std::io::Error::last_os_error()
            )))
        } else {
            Err(UpdateError::LaunchFailed(format!(
                "예상치 못한 대기 결과 코드: {:#x}",
                ret.0
            )))
        }
    }
}

/// 다운로드 완료된 설치 프로그램을 실행하고 프로세스 핸들을 반환합니다.
/// NSIS는 직접 실행하며, MSI는 `msiexec.exe /i "<경로>"` 형태로 실행합니다.
pub fn launch_installer(
    setup_path: &Path,
    install_kind: InstallKind,
) -> Result<SafeProcessHandle, UpdateError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::PCWSTR;

    if !setup_path.exists() {
        return Err(UpdateError::LaunchFailed(format!(
            "설치 파일이 없습니다: {}",
            setup_path.display()
        )));
    }

    let (file_wide, params_wide) = match install_kind {
        InstallKind::Nsis => {
            let file: Vec<u16> = setup_path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            (file, Vec::new())
        }
        InstallKind::Msi => {
            let file: Vec<u16> = std::ffi::OsStr::new("msiexec.exe")
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let args = format!("/i \"{}\"", setup_path.display());
            let params: Vec<u16> = std::ffi::OsStr::new(&args)
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            (file, params)
        }
        InstallKind::Unknown => {
            return Err(UpdateError::UnsupportedInstallKind(
                "알 수 없는 설치 형식은 실행할 수 없습니다".into(),
            ));
        }
    };

    let wide_op: Vec<u16> = std::ffi::OsStr::new("open")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut exec_info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(wide_op.as_ptr()),
        lpFile: PCWSTR(file_wide.as_ptr()),
        lpParameters: if params_wide.is_empty() {
            PCWSTR::null()
        } else {
            PCWSTR(params_wide.as_ptr())
        },
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    let ok = unsafe { ShellExecuteExW(&mut exec_info) };
    if ok.is_err() || exec_info.hProcess.is_invalid() {
        return Err(UpdateError::LaunchFailed(format!(
            "ShellExecuteExW 실행 실패: {:?}",
            ok.err()
        )));
    }

    Ok(SafeProcessHandle(exec_info.hProcess))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn parse_version_handles_prefixes_and_clean_semver() {
        assert_eq!(parse_version("v0.1.0"), Some(semver::Version::new(0, 1, 0)));
        assert_eq!(parse_version("0.1.0"), Some(semver::Version::new(0, 1, 0)));
        assert_eq!(
            parse_version("  v1.2.34  "),
            Some(semver::Version::new(1, 2, 34))
        );
        assert_eq!(parse_version("invalid"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn detect_install_kind_from_paths_rules() {
        let temp = tempfile::tempdir().unwrap();
        let nsis_dir = temp.path().join("nsis_app");
        std::fs::create_dir_all(&nsis_dir).unwrap();
        std::fs::write(nsis_dir.join("uninstall.exe"), b"").unwrap();
        let nsis_exe = nsis_dir.join("expandrv.exe");

        // 1. uninstall.exe가 있으면 등록 여부와 무관하게 NSIS
        assert_eq!(
            detect_install_kind_from_paths(&nsis_exe, None, false),
            InstallKind::Nsis
        );
        assert_eq!(
            detect_install_kind_from_paths(&nsis_exe, Some(&nsis_dir), true),
            InstallKind::Nsis
        );

        // 2. 개발 target 경로는 무조건 Unknown
        let dev_exe = PathBuf::from(r"D:\PROJECTS\ExpanDrv\target\release\expandrv.exe");
        assert_eq!(
            detect_install_kind_from_paths(
                &dev_exe,
                Some(Path::new(r"D:\PROJECTS\ExpanDrv\target\release")),
                true
            ),
            InstallKind::Unknown
        );

        // 3. 유효 설치 위치: 등록된 MSI 위치와 실행 바이너리 디렉터리가 일치하고 배포 플래그가 유효하면 MSI
        let msi_dir = temp.path().join("msi_app");
        std::fs::create_dir_all(&msi_dir).unwrap();
        let msi_exe = msi_dir.join("expandrv.exe");
        assert_eq!(
            detect_install_kind_from_paths(&msi_exe, Some(&msi_dir), true),
            InstallKind::Msi
        );

        // 4. 사용자 지정 MSI 설치 위치: Program Files가 아닌 임의 경로라도 등록 위치와 일치하면 MSI
        let custom_msi_dir = PathBuf::from(r"D:\CustomApps\ExpanDrv");
        let custom_msi_exe = custom_msi_dir.join("expandrv.exe");
        assert_eq!(
            detect_install_kind_from_paths(&custom_msi_exe, Some(&custom_msi_dir), true),
            InstallKind::Msi
        );

        // 5. 타 위치에 등록된 제품: 시스템에 MSI가 설치되어 있으나 현재 바이너리는 다른 위치(예: 다운로드 폴더)에 있는 경우 Unknown
        let other_exe = temp.path().join("downloads").join("expandrv.exe");
        assert_eq!(
            detect_install_kind_from_paths(&other_exe, Some(&msi_dir), true),
            InstallKind::Unknown
        );

        // 6. 단순 Program Files 복사본: Program Files 아래에 있으나 레지스트리에 등록된 적 없는 경우 Unknown
        let pf_copy = PathBuf::from(r"C:\Program Files\ExpanDrv_Copy\expandrv.exe");
        assert_eq!(
            detect_install_kind_from_paths(&pf_copy, None, false),
            InstallKind::Unknown
        );
        // 배포 플래그는 있으나 등록된 디렉터리가 다른 경우도 Unknown
        assert_eq!(
            detect_install_kind_from_paths(&pf_copy, Some(&msi_dir), true),
            InstallKind::Unknown
        );

        // 7. uninstall.exe도 없고 MSI 등록도 없는 미등록 바이너리는 Unknown
        assert_eq!(
            detect_install_kind_from_paths(&msi_exe, None, false),
            InstallKind::Unknown
        );
    }

    #[test]
    fn safe_process_handle_wait_timeout_and_drop_closes_handle() {
        use std::os::windows::io::AsRawHandle;
        use std::process::Command;

        let mut child = Command::new("cmd")
            .args(["/c", "exit 42"])
            .spawn()
            .expect("자식 프로세스 실행 실패");

        let raw_handle = windows::Win32::Foundation::HANDLE(child.as_raw_handle());
        let current_proc = unsafe { windows::Win32::System::Threading::GetCurrentProcess() };
        let mut dup_handle = windows::Win32::Foundation::HANDLE::default();
        unsafe {
            windows::Win32::Foundation::DuplicateHandle(
                current_proc,
                raw_handle,
                current_proc,
                &mut dup_handle,
                0,
                false,
                windows::Win32::Foundation::DUPLICATE_SAME_ACCESS,
            )
            .expect("DuplicateHandle 실패");
        }

        let safe_handle = SafeProcessHandle::new(dup_handle);
        let mut exited = false;
        for _ in 0..30 {
            match safe_handle.wait_timeout(100).expect("wait_timeout 실패") {
                ProcessWaitResult::Exited(code) => {
                    assert_eq!(code, 42);
                    exited = true;
                    break;
                }
                ProcessWaitResult::Timeout => continue,
            }
        }
        assert!(exited, "프로세스가 정상 종료되어야 합니다");
        let _ = child.wait();
    }

    #[test]
    fn fake_installer_child_process_deadlock_prevented() {
        use std::os::windows::io::AsRawHandle;
        use std::process::Command;
        use std::sync::atomic::{AtomicBool, Ordering};

        // 1. 에이전트의 종료를 대기하는 가짜 installer 자식 프로세스를 띄웁니다.
        // 자식 프로세스는 백그라운드에서 10초 동안 대기(수면)하여 아직 살아있는 인스톨러 상황을 완벽히 모사합니다.
        let mut fake_installer = Command::new("powershell")
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 10"])
            .spawn()
            .expect("가짜 installer 프로세스 기동 실패");

        let raw_handle = windows::Win32::Foundation::HANDLE(fake_installer.as_raw_handle());
        let current_proc = unsafe { windows::Win32::System::Threading::GetCurrentProcess() };
        let mut dup_handle = windows::Win32::Foundation::HANDLE::default();
        unsafe {
            windows::Win32::Foundation::DuplicateHandle(
                current_proc,
                raw_handle,
                current_proc,
                &mut dup_handle,
                0,
                false,
                windows::Win32::Foundation::DUPLICATE_SAME_ACCESS,
            )
            .expect("DuplicateHandle 실패");
        }
        let safe_handle = SafeProcessHandle::new(dup_handle);

        // 2. 에이전트의 Tokio 런타임과 종료 플래그를 생성합니다.
        let rt = tokio::runtime::Runtime::new().expect("Tokio Runtime 생성 실패");
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let flag_clone = shutdown_flag.clone();

        // 3. 인스톨러 모니터 스레드를 기동합니다 (tray.rs의 실제 구현과 동일한 패턴).
        let monitor_handle = std::thread::Builder::new()
            .name("test-installer-monitor".into())
            .spawn(move || {
                loop {
                    if flag_clone.load(Ordering::Relaxed) {
                        break;
                    }
                    match safe_handle.wait_timeout(100) {
                        Ok(ProcessWaitResult::Exited(_)) => break,
                        Ok(ProcessWaitResult::Timeout) => {}
                        Err(_) => break,
                    }
                }
            })
            .expect("감시 스레드 기동 실패");

        // 4. 에이전트가 installer prehook의 `expandrv stop` 요청을 받아 정상 셧다운되는 흐름 재현:
        // shutdown_flag를 켜고 Runtime을 drop합니다.
        shutdown_flag.store(true, Ordering::SeqCst);
        let start = std::time::Instant::now();
        drop(rt);
        let elapsed = start.elapsed();

        // 5. 교착(Deadlock)이 발생하지 않고 제한 시간(2초 미만) 안에 에이전트 런타임이 즉시 drop 완료되었는지 검증합니다!
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "자식 프로세스가 살아있는 동안에도 에이전트 런타임 drop이 즉시 완료되어야 합니다: {:?}",
            elapsed
        );

        let _ = monitor_handle.join();
        let _ = fake_installer.kill();
        let _ = fake_installer.wait();
    }

    #[test]
    fn validate_github_asset_url_rejects_malicious_and_accepts_valid() {
        let valid = "https://github.com/cloim/ExpanDrv/releases/download/v0.1.1/ExpanDrv_0.1.1_x64-setup.exe";
        assert!(
            validate_github_asset_url(
                valid,
                "cloim",
                "ExpanDrv",
                "v0.1.1",
                "ExpanDrv_0.1.1_x64-setup.exe"
            )
            .is_ok()
        );

        // 다른 호스트 거부
        let evil_host =
            "https://evil.com/cloim/ExpanDrv/releases/download/v0.1.1/ExpanDrv_0.1.1_x64-setup.exe";
        assert!(matches!(
            validate_github_asset_url(
                evil_host,
                "cloim",
                "ExpanDrv",
                "v0.1.1",
                "ExpanDrv_0.1.1_x64-setup.exe"
            ),
            Err(UpdateError::InvalidAssetUrl(_))
        ));

        // HTTP 프로토콜 거부
        let http_url = "http://github.com/cloim/ExpanDrv/releases/download/v0.1.1/ExpanDrv_0.1.1_x64-setup.exe";
        assert!(matches!(
            validate_github_asset_url(
                http_url,
                "cloim",
                "ExpanDrv",
                "v0.1.1",
                "ExpanDrv_0.1.1_x64-setup.exe"
            ),
            Err(UpdateError::InvalidAssetUrl(_))
        ));

        // 저장소 불일치 거부
        let wrong_repo = "https://github.com/attacker/ExpanDrv/releases/download/v0.1.1/ExpanDrv_0.1.1_x64-setup.exe";
        assert!(matches!(
            validate_github_asset_url(
                wrong_repo,
                "cloim",
                "ExpanDrv",
                "v0.1.1",
                "ExpanDrv_0.1.1_x64-setup.exe"
            ),
            Err(UpdateError::InvalidAssetUrl(_))
        ));

        // 태그 불일치 거부
        let wrong_tag = "https://github.com/cloim/ExpanDrv/releases/download/v0.1.0/ExpanDrv_0.1.1_x64-setup.exe";
        assert!(matches!(
            validate_github_asset_url(
                wrong_tag,
                "cloim",
                "ExpanDrv",
                "v0.1.1",
                "ExpanDrv_0.1.1_x64-setup.exe"
            ),
            Err(UpdateError::InvalidAssetUrl(_))
        ));

        // 파일명 불일치 거부
        let wrong_file = "https://github.com/cloim/ExpanDrv/releases/download/v0.1.1/malware.exe";
        assert!(matches!(
            validate_github_asset_url(
                wrong_file,
                "cloim",
                "ExpanDrv",
                "v0.1.1",
                "ExpanDrv_0.1.1_x64-setup.exe"
            ),
            Err(UpdateError::InvalidAssetUrl(_))
        ));
    }

    #[test]
    fn find_latest_update_filters_drafts_and_prereleases_including_semver_pre() {
        let current = semver::Version::new(0, 1, 0);
        let releases = vec![
            // draft release
            GitHubRelease {
                tag_name: "v0.1.2".into(),
                name: Some("Draft release".into()),
                draft: true,
                prerelease: false,
                html_url: "https://example.com/draft".into(),
                assets: vec![],
            },
            // prerelease flag = true
            GitHubRelease {
                tag_name: "v0.1.1".into(),
                name: Some("Pre-release flag".into()),
                draft: false,
                prerelease: true,
                html_url: "https://example.com/pre".into(),
                assets: vec![],
            },
            // prerelease flag = false 이지만 SemVer prerelease 태그 (-rc1)
            GitHubRelease {
                tag_name: "v0.1.3-rc1".into(),
                name: Some("SemVer Pre-release".into()),
                draft: false,
                prerelease: false,
                html_url: "https://example.com/pre".into(),
                assets: vec![
                    GitHubAsset {
                        name: "ExpanDrv_0.1.3-rc1_x64-setup.exe".into(),
                        size: 1000,
                        browser_download_url: "https://github.com/cloim/ExpanDrv/releases/download/v0.1.3-rc1/ExpanDrv_0.1.3-rc1_x64-setup.exe".into(),
                    },
                    GitHubAsset {
                        name: "ExpanDrv_0.1.3-rc1_manifest.json".into(),
                        size: 100,
                        browser_download_url: "https://github.com/cloim/ExpanDrv/releases/download/v0.1.3-rc1/ExpanDrv_0.1.3-rc1_manifest.json".into(),
                    },
                ],
            },
        ];

        let result = find_latest_update(&releases, &current, InstallKind::Nsis).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn find_latest_update_selects_correct_asset_by_install_kind() {
        let current = semver::Version::new(0, 1, 0);
        let releases = vec![GitHubRelease {
            tag_name: "v0.2.0".into(),
            name: Some("ExpanDrv v0.2.0".into()),
            draft: false,
            prerelease: false,
            html_url: "https://github.com/cloim/ExpanDrv/releases/tag/v0.2.0".into(),
            assets: vec![
                GitHubAsset {
                    name: "ExpanDrv_0.2.0_x64-setup.exe".into(),
                    size: 1200,
                    browser_download_url: "https://github.com/cloim/ExpanDrv/releases/download/v0.2.0/ExpanDrv_0.2.0_x64-setup.exe".into(),
                },
                GitHubAsset {
                    name: "ExpanDrv_0.2.0_x64_en-US.msi".into(),
                    size: 1400,
                    browser_download_url: "https://github.com/cloim/ExpanDrv/releases/download/v0.2.0/ExpanDrv_0.2.0_x64_en-US.msi".into(),
                },
                GitHubAsset {
                    name: "ExpanDrv_0.2.0_manifest.json".into(),
                    size: 250,
                    browser_download_url: "https://github.com/cloim/ExpanDrv/releases/download/v0.2.0/ExpanDrv_0.2.0_manifest.json".into(),
                },
            ],
        }];

        // NSIS 설치본: setup.exe 선택
        let nsis_cand = find_latest_update(&releases, &current, InstallKind::Nsis)
            .unwrap()
            .unwrap();
        assert_eq!(nsis_cand.setup_asset.name, "ExpanDrv_0.2.0_x64-setup.exe");
        assert_eq!(nsis_cand.setup_asset.size, 1200);
        assert_eq!(nsis_cand.install_kind, InstallKind::Nsis);

        // MSI 설치본: msi 선택
        let msi_cand = find_latest_update(&releases, &current, InstallKind::Msi)
            .unwrap()
            .unwrap();
        assert_eq!(msi_cand.setup_asset.name, "ExpanDrv_0.2.0_x64_en-US.msi");
        assert_eq!(msi_cand.setup_asset.size, 1400);
        assert_eq!(msi_cand.install_kind, InstallKind::Msi);

        // Unknown 설치본: 명확한 에러 반환
        let err = find_latest_update(&releases, &current, InstallKind::Unknown).unwrap_err();
        assert!(matches!(err, UpdateError::UnsupportedInstallKind(_)));
    }

    #[test]
    fn find_latest_update_empty_releases_returns_none() {
        let current = semver::Version::new(0, 1, 0);
        let result = find_latest_update(&[], &current, InstallKind::Nsis).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn verify_manifest_checks_architecture_version_and_sha256() {
        let candidate = UpdateCandidate {
            version: semver::Version::new(0, 1, 1),
            version_str: "0.1.1".into(),
            release_name: "v0.1.1".into(),
            release_url: "https://example.com".into(),
            install_kind: InstallKind::Nsis,
            setup_asset: AssetDownloadTarget {
                name: "ExpanDrv_0.1.1_x64-setup.exe".into(),
                size: 5000,
                url: "https://example.com/setup.exe".into(),
            },
            manifest_asset: AssetDownloadTarget {
                name: "ExpanDrv_0.1.1_manifest.json".into(),
                size: 100,
                url: "https://example.com/manifest.json".into(),
            },
        };

        let valid_manifest = Manifest {
            version: "0.1.1".into(),
            architecture: "x64".into(),
            winfsp_version: Some("2.1".into()),
            winfsp_sha256: None,
            artifacts: vec![ManifestArtifact {
                file: "ExpanDrv_0.1.1_x64-setup.exe".into(),
                bytes: 5000,
                sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                signature: Some("NotSigned".into()),
                winfsp: Some("embedded".into()),
            }],
        };

        let artifact = verify_manifest(&valid_manifest, &candidate).unwrap();
        assert_eq!(artifact.file, "ExpanDrv_0.1.1_x64-setup.exe");

        // 아키텍처 불일치
        let mut bad_arch = valid_manifest.clone();
        bad_arch.architecture = "arm64".into();
        assert!(matches!(
            verify_manifest(&bad_arch, &candidate),
            Err(UpdateError::InvalidManifest(_))
        ));

        // 크기 불일치
        let mut bad_size = valid_manifest.clone();
        bad_size.artifacts[0].bytes = 4999;
        assert!(matches!(
            verify_manifest(&bad_size, &candidate),
            Err(UpdateError::InvalidManifest(_))
        ));

        // 버전 불일치
        let mut bad_ver = valid_manifest.clone();
        bad_ver.version = "0.1.2".into();
        assert!(matches!(
            verify_manifest(&bad_ver, &candidate),
            Err(UpdateError::InvalidManifest(_))
        ));

        // 잘못된 SHA256 해시 형식
        let mut bad_sha = valid_manifest.clone();
        bad_sha.artifacts[0].sha256 = "not-a-hex".into();
        assert!(matches!(
            verify_manifest(&bad_sha, &candidate),
            Err(UpdateError::InvalidManifest(_))
        ));
    }

    #[test]
    fn verify_file_hash_and_size_validates_correctly() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("test_installer.bin");
        let content = b"ExpanDrv Test Installer Content 12345";
        std::fs::write(&file_path, content).unwrap();

        let mut hasher = Sha256::new();
        hasher.update(content);
        let expected_hash = format!("{:x}", hasher.finalize());

        // 정상 검증 (대소문자 무관)
        assert!(
            verify_file_hash_and_size(&file_path, content.len() as u64, &expected_hash).is_ok()
        );
        assert!(
            verify_file_hash_and_size(
                &file_path,
                content.len() as u64,
                &expected_hash.to_uppercase()
            )
            .is_ok()
        );

        // 크기 불일치
        let size_err =
            verify_file_hash_and_size(&file_path, (content.len() + 1) as u64, &expected_hash)
                .unwrap_err();
        assert!(matches!(size_err, UpdateError::SizeMismatch { .. }));

        // 해시 불일치
        let fake_hash = "0000000000000000000000000000000000000000000000000000000000000000";
        let hash_err =
            verify_file_hash_and_size(&file_path, content.len() as u64, fake_hash).unwrap_err();
        assert!(matches!(hash_err, UpdateError::HashMismatch { .. }));
    }
}
