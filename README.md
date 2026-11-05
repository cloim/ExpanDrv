# ExpanDrv

원격 스토리지를 Windows 드라이브 문자 또는 폴더로 마운트하는 Rust 네이티브 앱입니다. 탐색기와 일반 프로그램에서 로컬 디스크처럼 읽고 쓸 수 있습니다.

> 개발 초기 단계입니다. 현재 CLI로 마운트할 수 있고, GUI(Tauri 2), 자격 증명 암호화 저장, OAuth 클라우드는 개발 중입니다.

## 지원 스토리지

| 종류 | 상태 | 비고 |
|---|---|---|
| 로컬 폴더 | 지원 | 테스트와 검증용 |
| WebDAV | 지원 | Range 읽기, 스트리밍 업로드, 자체 서명 인증서 SHA-256 고정 |
| SFTP | 지원 | 암호와 OpenSSH 개인키(암호문 포함), known_hosts 검증 |
| S3 호환 | 지원 | 경로 방식과 가상 호스트 방식, 멀티파트 업로드 |
| FTP, FTPS | 지원 | 명시적과 암묵적 TLS, 서버 문자셋 선택 |
| OneDrive, Google Drive 등 | 계획 | OAuth 2.0 PKCE |

## 특징

- **성능:** 4MiB 블록 캐시와 순차 읽기 선행, 디렉터리 목록 캐시, 파일을 닫은 뒤 백그라운드 업로드(write-back)
- **안정성:** 업로드 중 파일이 다시 바뀌면 진행 중 업로드를 취소하고 다시 올립니다. 마운트 해제 때 남은 업로드를 끝까지 기다립니다.
- **오프라인 탐색:** 원격이 응답하지 않으면 마지막으로 받은 디렉터리 목록(SQLite)으로 탐색을 이어갑니다.
- **원격 변경 반영:** 최근 연 폴더를 주기적으로 확인하고, 바뀐 내용을 탐색기 변경 알림으로 전달합니다.
- **보안:**
  - 사용자 세션 권한으로 실행하며 관리자 권한이 필요 없습니다.
  - 마운트한 볼륨은 현재 사용자, SYSTEM, Administrators만 접근할 수 있습니다.
  - 설정 파일에는 비밀 값을 저장하지 않습니다. 현재는 환경 변수 참조만 지원하고, DPAPI와 Windows 자격 증명 관리자를 추가할 예정입니다.
  - TLS는 rustls(ring)로 처리하고 TLS 1.2 이상만 허용합니다. 텔레메트리는 없습니다.
- **전송 제한:** 드라이브별 업로드와 다운로드 속도 제한

## 요구 사항

- Windows 10 이상 x64
- [WinFsp](https://winfsp.dev/) 2.x 런타임 설치(개발용 SDK는 필요 없음)
- 빌드할 때:
  - Rust 1.88 이상(MSVC 툴체인)
  - LLVM의 `libclang.dll`(WinFsp 바인딩 생성용). `LIBCLANG_PATH`로 위치를 지정할 수 있습니다.

## 빌드

```powershell
cargo build --release -p expandrv-agent
# 결과: target\release\expandrv.exe
```

## 사용법

```powershell
# WebDAV 드라이브 추가(암호는 환경 변수 이름으로만 기록)
$env:NAS_PASSWORD = '...'
expandrv add nas W: --label NAS webdav https://dav.example.com/remote.php/dav/files/me/ --user me --password-env NAS_PASSWORD

# SFTP(개인키와 known_hosts 검증)
expandrv add server S: sftp sftp.example.com --user me --key $HOME\.ssh\id_ed25519

# S3 호환
expandrv add bucket B: s3 https://s3.example.com my-bucket --access-key-id AKIA... --secret-env S3_SECRET

expandrv list             # 드라이브와 마운트 상태
expandrv check nas        # 연결과 인증만 확인
expandrv mount            # 전체 마운트(Ctrl+C로 해제)
expandrv unmount nas      # 다른 터미널에서 해제 요청
```

설정 파일은 `%APPDATA%\ExpanDrv\config.toml`이고, 캐시와 업로드 대기 파일은 `%LOCALAPPDATA%\ExpanDrv\cache\<id>`에 있습니다. 드라이브별로 다음 값을 조정할 수 있습니다.

```toml
[[drive]]
id = "nas"
label = "NAS"
mount_point = "W:"

[drive.cache]
metadata_ttl_secs = 30     # 목록과 속성 캐시 유지 시간
block_size_mib = 4         # 읽기 블록 크기
memory_limit_mib = 64      # 블록 캐시 메모리 상한
poll_interval_secs = 60    # 원격 변경 확인 주기(0이면 끔)

[drive.transfer]
upload_kib_per_sec = 0     # 0이면 제한 없음
download_kib_per_sec = 0

[drive.backend]
type = "webdav"
url = "https://dav.example.com/"
username = "me"
password = { from = "env", var = "NAS_PASSWORD" }
```

로그는 `EXPANDRV_LOG` 환경 변수로 조정합니다(예: `EXPANDRV_LOG=expandrv=debug`).

## 개발

```powershell
pwsh -File scripts/check.ps1   # fmt, clippy -D warnings, test
```

- `cargo test`의 마운트 통합 테스트는 WinFsp 런타임이 있을 때만 실제 드라이브 문자로 실행됩니다.
- 네트워크 백엔드 통합 테스트는 `EXPANDRV_TEST_*` 환경 변수가 있을 때만 실행됩니다. 각 테스트 파일 머리 주석에 Docker로 서버를 띄우는 방법이 있습니다.

| 크레이트 | 역할 |
|---|---|
| `expandrv-core` | 원격 경로, 오류 모델, `Backend` 트레이트, 설정 |
| `expandrv-backends` | Local, WebDAV, SFTP, S3, FTP 백엔드 |
| `expandrv-vfs` | 핸들 테이블, 메타와 블록 캐시, write-back 업로드, 변경 감지 |
| `expandrv-winfsp` | WinFsp 바인딩, 보안 서술자, 마운트 |
| `expandrv-agent` | `expandrv` CLI |

## 라이선스

[GNU General Public License v3.0](LICENSE)(`GPL-3.0-only`)

- 파일 시스템 계층은 [WinFsp](https://github.com/winfsp/winfsp)(GPLv3, FLOSS 예외)와 Rust 바인딩 [winfsp-rs](https://github.com/SnowflakePowered/winfsp-rs)(GPL-3.0)를 사용합니다.
- 그 밖의 의존성은 MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0 등 GPL-3.0과 호환되는 라이선스입니다.
