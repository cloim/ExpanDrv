# ExpanDrv

원격 스토리지를 Windows 드라이브 문자 또는 폴더로 마운트하는 Rust 네이티브 앱입니다. 탐색기와 일반 프로그램에서 로컬 디스크처럼 읽고 쓸 수 있습니다.

> 개발 초기 단계입니다. CLI, 상주 에이전트와 데스크톱 GUI(Tauri 2)로 마운트할 수 있고, 트레이 아이콘과 OAuth 클라우드는 개발 중입니다.

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

- **성능:** 4MiB 블록 단위 읽기와 순차 읽기 선행, 메모리와 암호화 디스크 블록 캐시(재시작 뒤 다시 받지 않음), 디렉터리 목록 캐시, 파일을 닫은 뒤 백그라운드 업로드(write-back)
- **안정성:** 업로드 중 파일이 다시 바뀌면 진행 중 업로드를 취소하고 다시 올립니다. 마운트 해제 때 남은 업로드를 기다리고, 끝내지 못했거나 프로그램이 비정상 종료되면 업로드 저널로 다음 마운트 때 이어 올립니다. 그사이 원격 파일이 바뀌었으면 덮어쓰지 않고 `이름 (ExpanDrv 충돌 날짜 시각).확장자` 사본으로 올립니다.
- **다운로드 안정성:** 네트워크 끊김 같은 일시 오류는 자동으로 다시 시도하고, 수정하려고 내려받던 큰 파일은 끊긴 위치부터 이어받습니다.
- **오프라인 탐색:** 원격이 응답하지 않으면 마지막으로 받은 디렉터리 목록(SQLite)으로 탐색을 이어가고, 최근에 읽은 파일은 디스크 캐시로 엽니다. 처음 마운트할 때는 원격 연결이 필요합니다.
- **원격 변경 반영:** 최근 연 폴더를 주기적으로 확인하고, 바뀐 내용을 탐색기 변경 알림으로 전달합니다.
- **보안:**
  - 사용자 세션 권한으로 실행하며 관리자 권한이 필요 없습니다.
  - 마운트한 볼륨은 현재 사용자, SYSTEM, Administrators만 접근할 수 있습니다.
  - 설정 파일에는 비밀 값을 저장하지 않습니다. 비밀번호는 DPAPI(현재 사용자)로 한 번 더 암호화해 Windows 자격 증명 관리자에 저장하고, 설정에는 참조만 남깁니다. 환경 변수 참조도 쓸 수 있습니다.
  - 디스크 캐시는 드라이브별 키로 AES-256-GCM 암호화하고 파일 이름도 해시로 숨깁니다. 키는 자격 증명 관리자에 두며, 캐시 폴더는 현재 사용자와 SYSTEM만 접근할 수 있습니다.
  - 에이전트 제어 파이프는 현재 사용자만 열 수 있고, 원격 연결을 거부하며 양쪽 프로세스의 사용자를 서로 확인합니다.
  - 로그에 섞인 비밀번호, 토큰, URL 자격 증명, 서명 값, 사용자 계정 이름을 출력 직전에 가립니다.
  - TLS는 rustls(ring)로 처리하고 TLS 1.2 이상만 허용합니다. 텔레메트리는 없습니다.
- **전송 제한과 프록시:** 드라이브별 업로드와 다운로드 속도 제한, SOCKS5(socks5h)와 HTTP CONNECT 프록시(FTP 데이터 연결 포함)
- **자동 실행:** 로그인할 때 에이전트를 시작하고 지정한 드라이브를 자동으로 마운트

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

GUI는 Node.js 24 이상이 더 필요합니다. 화면을 빌드한 뒤 실행 파일을 만들고, `expandrv.exe`와 같은 폴더에 둡니다(GUI가 에이전트를 시작할 때 이 CLI를 씁니다).

```powershell
cd gui
npm ci
npm run tauri -- build --no-bundle
# 결과: target\release\expandrv-gui.exe
```

`npm run build` 전에 `cargo build --workspace`를 하면 `gui/dist`에 안내 페이지만 들어갑니다.

## 사용법

```powershell
# WebDAV 드라이브 추가: 암호를 화면에 보이지 않게 입력받아 자격 증명 관리자에 저장
expandrv add nas W: --label NAS --auto-mount webdav https://dav.example.com/remote.php/dav/files/me/ --user me --ask-password

# SFTP(개인키와 known_hosts 검증, 키 암호문 입력)
expandrv add server S: sftp sftp.example.com --user me --key $HOME\.ssh\id_ed25519 --ask-key-passphrase

# S3 호환(비밀 키 입력), 프록시 경유
expandrv add bucket B: --proxy socks5h://proxy.example.com:1080 s3 https://s3.example.com my-bucket --access-key-id AKIA... --ask-secret

# 환경 변수 참조도 쓸 수 있습니다(--password-env, --secret-env 등)
expandrv add ftp1 F: ftp ftp.example.com --user me --password-env FTP_PASSWORD

expandrv list               # 드라이브와 마운트 상태
expandrv check nas          # 연결과 인증만 확인
expandrv mount              # 전체 마운트(에이전트가 없으면 백그라운드로 시작)
expandrv unmount nas        # 남은 업로드를 마친 뒤 해제
expandrv stop               # 모두 해제하고 에이전트 종료
expandrv secret set nas password    # 저장한 암호 바꾸기
expandrv secret list                # 저장한 비밀 값 대상 이름(값은 보이지 않음)
expandrv autostart enable           # 로그인할 때 시작하고 --auto-mount 드라이브 마운트
```

`--ask-*` 옵션은 터미널이면 화면에 보이지 않게 입력받고, 파이프로 실행하면 표준 입력 한 줄을 읽습니다. 저장 위치는 `ExpanDrv/<드라이브 id>/<항목>`이고, `expandrv remove`로 드라이브를 지우면 함께 지워집니다.

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
disk_limit_mib = 1024      # 암호화 디스크 블록 캐시 상한(0이면 끔)

[drive.transfer]
upload_kib_per_sec = 0     # 0이면 제한 없음
download_kib_per_sec = 0

[drive.backend]
type = "webdav"
url = "https://dav.example.com/"
username = "me"
password = { from = "env", var = "NAS_PASSWORD" }
```

`--ask-password`로 추가하면 `password = { from = "credential", target = "ExpanDrv/nas/password" }`처럼 기록됩니다. 드라이브별 `auto_mount = true`와 `[drive.proxy]`(`url`, `username`, `password`)도 설정할 수 있습니다.

로그는 `EXPANDRV_LOG` 환경 변수로 조정합니다(예: `EXPANDRV_LOG=expandrv=debug`). 백그라운드 에이전트 로그는 `%LOCALAPPDATA%\ExpanDrv\logs`에 하루 단위로 7개까지 남습니다.

## 개발

```powershell
pwsh -File scripts/check.ps1   # fmt, clippy -D warnings, test
```

- GUI 화면은 `gui`에서 `npm run lint`(ESLint, 형식 검사)와 `npm run build`로 확인합니다. `npm run tauri -- dev -- --no-default-features`로 Vite 개발 서버와 함께 띄웁니다.
- `cargo test`의 마운트 통합 테스트는 WinFsp 런타임이 있을 때만 실제 드라이브 문자로 실행됩니다.
- 네트워크 백엔드 통합 테스트는 `EXPANDRV_TEST_*` 환경 변수가 있을 때만 실행됩니다. 각 테스트 파일 머리 주석에 Docker로 서버를 띄우는 방법이 있습니다.

| 크레이트 | 역할 |
|---|---|
| `expandrv-core` | 원격 경로, 오류 모델, `Backend` 트레이트, 설정 |
| `expandrv-backends` | Local, WebDAV, SFTP, S3, FTP 백엔드 |
| `expandrv-vfs` | 핸들 테이블, 메타와 블록 캐시, write-back 업로드, 변경 감지 |
| `expandrv-winfsp` | WinFsp 바인딩, 보안 서술자, 마운트 |
| `expandrv-secrets` | DPAPI와 Windows 자격 증명 관리자 비밀 값 저장소 |
| `expandrv-ipc` | 소유자 전용 이름 있는 파이프 IPC(GUI와 CLI가 에이전트를 제어) |
| `expandrv-log` | 로그 비밀 값과 개인정보 마스킹 |
| `expandrv-autostart` | 로그인 자동 실행 등록(현재 사용자 Run 키) |
| `expandrv-agent` | `expandrv` CLI와 상주 에이전트 |
| `gui` | 데스크톱 GUI(Tauri 2, React). `gui/src-tauri`가 `expandrv-gui` 크레이트 |

## 라이선스

[GNU General Public License v3.0](LICENSE)(`GPL-3.0-only`)

- 파일 시스템 계층은 [WinFsp](https://github.com/winfsp/winfsp)(GPLv3, FLOSS 예외)와 Rust 바인딩 [winfsp-rs](https://github.com/SnowflakePowered/winfsp-rs)(GPL-3.0)를 사용합니다.
- 그 밖의 의존성은 MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0 등 GPL-3.0과 호환되는 라이선스입니다.
