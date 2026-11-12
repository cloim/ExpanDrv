# ExpanDrv

원격 스토리지를 Windows 드라이브 문자 또는 폴더로 마운트하는 Rust 네이티브 앱입니다. 탐색기와 일반 프로그램에서 로컬 디스크처럼 읽고 쓸 수 있습니다.

> CLI, 상주 에이전트와 데스크톱 GUI(Tauri 2), 트레이 상태와 알림, PIN 잠금과 Windows 자동 잠금을 지원합니다. Windows 설치 EXE와 MSI를 만들 수 있습니다. OneDrive와 Google Drive OAuth 연동은 후속 작업입니다.

## 지원 스토리지

| 종류 | 상태 | 비고 |
|---|---|---|
| 로컬 폴더 | 지원 | 테스트와 검증용 |
| WebDAV | 지원 | Range 읽기, 스트리밍 업로드, 자체 서명 인증서 SHA-256 고정 |
| SFTP | 지원 | 암호와 OpenSSH 개인키(암호문 포함), known_hosts 검증 |
| S3 호환 | 지원 | 경로 방식과 가상 호스트 방식, 멀티파트 업로드 |
| FTP, FTPS | 지원 | 명시적과 암묵적 TLS, 인증서 SHA-256 고정, 서버 문자셋 선택 |
| OneDrive, Google Drive 등 | 계획 | OAuth 2.0 PKCE |

FTPS에서 `UnknownIssuer`가 표시되면 서버 인증서의 발급자를 Windows가 신뢰하지 못한 상태입니다. 드라이브 추가 또는 연결 정보 수정에서 서버 관리자에게 확인한 **인증서 SHA-256 지문**을 입력하면 해당 인증서만 허용합니다. 지문을 비우면 기존 Windows 신뢰 저장소 검증을 사용하며, 서버 인증서가 바뀌면 지문을 다시 확인해야 합니다. CLI에서는 `ftp --pin-sha256 <SHA-256 지문>`, 설정 파일에서는 FTP 백엔드의 `pinned_cert_sha256`을 사용합니다.

## 특징

- **성능:** 4MiB 블록 단위 읽기와 순차 읽기 선행, 메모리와 암호화 디스크 블록 캐시(재시작 뒤 다시 받지 않음), 디렉터리 목록 캐시, 파일을 닫은 뒤 백그라운드 업로드(write-back)
- **적응형 읽기와 요청 우선순위:** 실제 연속 읽기량에 따라 선행 읽기를 늘리고 짧은 헤더 조회와 작은 임의 읽기는 필요한 블록만 받습니다. 선행 읽기는 메모리 예산의 절반과 설정 상한 안에서 수행합니다. 드라이브별 원격 요청은 최대 4개, 업로드와 변경 확인과 선행 읽기 등 백그라운드 요청은 최대 2개로 제한해 사용자 조회에 연결 슬롯을 남깁니다. 진행 중인 업로드 스트림의 대역폭 자체를 선점하지는 않습니다.
- **용도별 설정:** 드라이브 속성의 캐시에서 `균형`, `문서 작업`, `대용량 미디어`, `데이터 절약`을 선택합니다. 캐시 용량과 원격 확인 주기를 표시하고 직접 조정도 지원합니다. 기존 전송 속도 제한은 유지하며 저장 후 다시 마운트하면 적용합니다.
- **연결 복구:** 인증, 인증서, 네트워크와 마운트 지점 오류에 다음 행동과 원문을 표시합니다. 연결 정보의 `수정`에서 접속과 루트 조회에 성공한 설정만 저장합니다. 먼저 마운트를 해제하고 다른 속성 변경을 저장하거나 취소하세요. 저장된 자격 증명은 유지하며 로컬 보관 파일이 있으면 기존 연결에서 먼저 업로드해야 합니다. 변경 전 캐시는 같은 캐시 상위 폴더의 `<id>.previous-<시각>`에 보관합니다. 이 보관 폴더는 캐시 자동 정리 대상과 구분됩니다.
- **안정성:** 업로드 중 파일이 다시 바뀌면 진행 중 업로드를 취소하고 다시 올립니다. 마운트 해제 때 남은 업로드를 기다리고, 끝내지 못했거나 프로그램이 비정상 종료되면 업로드 저널로 다음 마운트 때 이어 올립니다. 그사이 원격 파일이 바뀌었으면 덮어쓰지 않고 `이름 (ExpanDrv 충돌 날짜 시각).확장자` 사본으로 올립니다.
- **다운로드 안정성:** 네트워크 끊김 같은 일시 오류는 자동으로 다시 시도하고, 수정하려고 내려받던 큰 파일은 끊긴 위치부터 이어받습니다.
- **탐색기 응답성:** 드라이브 문자 마운트는 기본으로 네트워크 드라이브(`\\ExpanDrv-<사용자>\<드라이브 ID>`)로 등록합니다. 탐색기, 백신 실시간 검사와 검색 색인이 로컬 디스크처럼 파일 내용을 미리 읽지 않아 우클릭, 속성, 미리보기가 빨라집니다. 로컬 디스크로 보이게 하려면 설정 파일의 드라이브 항목에 `network_drive = false`를 지정합니다. 폴더 마운트는 항상 로컬 디스크로 등록합니다. 네트워크 드라이브에서는 탐색기 삭제가 휴지통을 거치지 않습니다. 또한 미리보기와 속성 처리기의 짧은 머리, 꼬리 조회는 4MiB 블록 대신 256KiB 조각만 받고, 연속 읽기가 1MiB를 넘으면 일반 블록 읽기와 선행 읽기로 전환합니다. 파일 끝 1MiB 안의 조회는 끝까지 받습니다.
- **FTP 연속 읽기:** 전송을 중간에 끊으면 제어 연결까지 닫는 서버(Synology 등)를 위해 1MiB 이상 읽은 전송을 끊지 않고 잠시 남겨 다음 블록을 같은 전송으로 이어 받습니다. 선행 읽기도 블록을 순서대로 요청합니다. 데이터가 3초 동안 오지 않으면 새 연결로 다시 받고, 끊긴 연결 대신 예비 연결을 미리 엽니다.
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
- [WinFsp](https://winfsp.dev/) 2.1 이상 런타임(설치 EXE에 포함하며 개발용 SDK는 필요 없음)
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

## Windows 설치 패키지와 릴리스 채널

프로젝트 루트에서 GUI 의존성을 설치한 뒤 실행합니다. Windows 빌드 도구, Rust와 Node.js가 필요하며 Tauri가 NSIS와 WiX 도구를 준비합니다.

```powershell
cd gui
npm ci
cd ..
pwsh -NoProfile -File scripts/package.ps1
# 결과: release/ExpanDrv_0.1.0_x64-setup.exe
#       release/ExpanDrv_0.1.0_x64_en-US.msi
#       release/ExpanDrv_0.1.0_manifest.json
```

일반 설치에는 EXE를 사용합니다. GUI와 CLI를 사용자 폴더에 설치하고 시작 메뉴와 제거 항목을 등록합니다. WinFsp 2.1 이상이 있으면 재사용합니다. 없으면 포함한 공식 MSI로 런타임을 설치하며 이때만 관리자 승인을 요청합니다. WebView2가 없는 환경은 포함된 부트스트래퍼가 런타임을 내려받으므로 인터넷 연결이 필요합니다.

MSI는 WinFsp가 이미 배포된 환경의 관리자 설치용입니다. WinFsp 2.1 이상이 없으면 설치를 중단하고 EXE 사용을 안내합니다. 두 형식을 같은 컴퓨터에 함께 설치하지 말고 배포 형식 하나를 선택하세요.

GitHub Releases(`cloim/ExpanDrv`)에 `v<버전>` 태그로 정식 릴리스를 게시할 때는 생성된 `ExpanDrv_<버전>_x64-setup.exe`, `ExpanDrv_<버전>_x64_en-US.msi`, `ExpanDrv_<버전>_manifest.json` 세 산출물을 첨부합니다. 매니페스트에는 패키지 파일명, 바이트 크기, SHA-256 해시와 서명 상태가 기록되며, 클라이언트 에이전트의 트레이 업데이트 채널이 이 계약을 바탕으로 다운로드 무결성을 검증합니다.

업그레이드와 제거 전에 에이전트를 종료하며 남은 업로드를 기다립니다. 종료에 실패하면 설치 작업을 중단합니다. 제거할 때 설정, 자격 증명, 캐시와 업로드 저널, 공용 WinFsp 런타임을 보존합니다. 탐색기 메뉴는 제거하는 실행 파일 경로에 연결된 항목만 해제합니다.

`installer/dependencies.json`의 WinFsp 버전, SHA-256과 배포자 전자서명을 확인한 뒤 패키지에 넣습니다. 원본 다운로드는 `gui/src-tauri/installer/deps/`에 보관합니다. 해시나 서명 확인에 실패하면 종료하므로 원본과 네트워크 상태를 확인한 뒤 다시 실행하세요. `release/`의 매니페스트에서 최종 파일 크기와 SHA-256, 서명 상태를 확인할 수 있습니다.

기본 산출물은 서명 전 테스트용입니다. 코드 서명 인증서를 준비하면 Windows SDK의 `signtool.exe`를 PATH에 추가하고 인증서 지문을 전달합니다. GUI, CLI와 설치 파일에 SHA-256 서명과 타임스탬프를 적용하고 최종 서명을 확인합니다.

```powershell
pwsh -NoProfile -File scripts/package.ps1 -CertificateThumbprint '<인증서 SHA1 지문 40자리>'
# 이미 검증한 최신 릴리스 실행 파일을 재포장할 때만 -SkipBuild 사용
```

패키지 설정이나 제거 훅을 변경하면 WinFsp 2.1 이상이 설치된 Windows에서 아래 검사를 실행합니다. 별도 테스트 제품명과 프로필로 설치, 업그레이드 중 실제 파일 업로드 완료, 제거와 데이터 보존을 확인합니다. 로그는 작업 실행기에, 판정은 `target/installer-smoke/result.json`에 남습니다. 실패하면 해당 테스트 설치와 로그를 먼저 확인하세요. WinFsp가 없는 새 Windows의 관리자 승인, 재부팅과 WebView2 최초 설치는 별도 가상 머신에서 확인합니다.

```powershell
pwsh -NoProfile -File scripts/test-installer.ps1
```

## 사용법

SFTP 서비스를 사용할 수 없는 서버는 GUI의 **SSH / SFTP → 연결 방식: SSH 셸**을 선택하세요. **로컬 SSH 설정 사용**을 켜고 `ds920` 같은 별칭을 입력하면 기존 SSH 주소·포트·계정·키로 연결합니다. SSH 명령 실행 권한과 서버의 Python 3.8 이상이 필요합니다. 연결마다 메모리에서 파일 도우미를 실행하며 서버 설치, SFTP 서비스 활성화나 관리자 권한은 필요하지 않습니다. 파일 목록, 부분 읽기, 스트리밍 업로드, 폴더 생성, 이름 변경, 삭제와 수정 시각을 지원합니다. SSH 셸 방식은 Linux 서버를 대상으로 검증하며 기본 연결 방식은 기존 SFTP입니다.

GUI에서 SFTP를 추가할 때 **로컬 SSH 설정 사용**을 켜면 `sfa3` 같은 SSH 별칭만으로 연결할 수 있습니다. 로컬 `ssh -G`로 `HostName`, `User`, `Port`, `IdentityFile`, `UserKnownHostsFile`을 가져오므로 `Include`와 `Host` 패턴도 적용됩니다. 존재하는 개인키들을 순서대로 시도하고 서버 키는 기존 known_hosts로 검증합니다. 암호화된 키는 개인키 암호문을 입력하세요. 별칭을 저장하므로 다음 마운트 때 로컬 설정을 다시 읽습니다. 로컬 OpenSSH가 필요하며 ssh-agent, ProxyJump, ProxyCommand, HostKeyAlias 및 SSH 알고리즘 설정 가져오기는 지원하지 않습니다.

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

설정 파일은 `~/.config/ExpanDrv/config.toml`이고, 캐시와 업로드 대기 파일은 `%LOCALAPPDATA%\ExpanDrv\cache\<id>`에 있습니다. 드라이브별로 다음 값을 조정할 수 있습니다.

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
readahead_blocks = 4       # 적응형 선행 읽기 상한(0~16, 0이면 끔)

[drive.transfer]
upload_kib_per_sec = 0     # 0이면 제한 없음
download_kib_per_sec = 0

[drive.backend]
type = "webdav"
url = "https://dav.example.com/"
username = "me"
password = { from = "env", var = "NAS_PASSWORD" }
```

`--ask-password`로 추가하면 `password = { from = "credential", target = "ExpanDrv/nas/password" }`처럼 기록됩니다. 드라이브별 `auto_mount = true`와 `[drive.proxy]`(`url`, `username`, `password`)도 설정할 수 있습니다. `auto_mount` 드라이브는 에이전트가 시작될 때마다 마운트합니다. 로그인 자동 실행, GUI 실행과 CLI 명령 중 어느 경로로 시작해도 같습니다.

로그는 `EXPANDRV_LOG` 환경 변수로 조정합니다(예: `EXPANDRV_LOG=expandrv=debug`). 백그라운드 에이전트 로그는 `%LOCALAPPDATA%\ExpanDrv\logs`에 하루 단위로 7개까지 남습니다.

## 트레이와 자동 업데이트

상주 에이전트가 알림 영역에 ExpanDrv 아이콘을 표시합니다. 아이콘에 포인터를 올리면 마운트 수, 업로드 대기 수와 오류 수를 볼 수 있습니다. 우클릭 메뉴는 업데이트 항목, 구분선, `열기`, `잠금`, `종료` 순서입니다. 두 번 클릭하거나 `열기`를 선택하면 같은 폴더의 `expandrv-gui.exe`를 실행합니다. `종료`는 남은 업로드를 마무리하고 에이전트를 종료합니다.

에이전트는 시작 10초 뒤와 6시간마다, 그리고 트레이 메뉴를 열 때(직전 확인 뒤 1분이 지났을 때) GitHub Releases 채널의 최신 정식 릴리스를 백그라운드에서 확인합니다. 메뉴가 열려 있는 동안 결과가 오면 업데이트 항목을 바로 고칩니다. 새 버전이 있으면 `업데이트 하기 (v<새버전>)`로 표시하고, 누르면 다운로드와 검증부터 설치 프로그램 실행까지 이어서 진행합니다. 새 버전이 없으면 `최신 버전 (v<현재버전>)`으로 표시합니다. 확인 중에는 `업데이트 확인 중...`, 진행 중에는 `업데이트 다운로드 중`과 `업데이트 설치 중`을 표시합니다. 개발 빌드처럼 설치 형식을 알 수 없으면 `업데이트 미지원`으로 표시합니다.
- **설치 형식 자동 식별과 혼용 방지:** 현재 실행 파일 디렉터리의 제거기(`uninstall.exe`) 및 레지스트리 배포 등록(`InstallDir` 및 `Deployment`)을 정밀 검사하여 NSIS 설치본에는 NSIS 설치 패키지(`ExpanDrv_<버전>_x64-setup.exe`)를, MSI 설치본에는 MSI 패키지(`ExpanDrv_<버전>_x64_en-US.msi`)를 일치시켜 선택합니다. 레지스트리에 등록된 실제 설치 경로와 현재 실행 위치를 대조하여 타 위치에 등록된 제품이나 단순 Program Files 복사본 등 미등록/식별 불가 경로는 업데이트를 안전하게 차단하며, 사용자 지정 위치에 설치된 MSI도 기존 설치 경로를 유지하도록 지원합니다.
- **안전한 릴리스 채널과 무결성 검증:** draft, prerelease 플래그 및 SemVer prerelease 태그는 정식 채널에서 제외합니다. 다운로드 URL은 고정 저장소(`cloim/ExpanDrv`), 태그 및 파일명과 연결해 검증하며, HTTPS를 유지하는 GitHub 공식 CDN 리다이렉트만 허용합니다. 다운로드 후 SHA-256 매니페스트와 파일 크기를 대조하여 검증에 실패한 파일은 즉시 삭제합니다.
- **단일 실행과 비동기 감시 인계:** 다운로드와 검증이 끝나면 즉시 설치 진행 상태(`Installing`)로 전이되어 중복 실행을 막습니다. 에이전트가 먼저 급하게 종료되지 않고 인스톨러를 시작하며, 인스톨러의 사전 실행 훅(`hooks.nsh` 또는 `requirements.wxs`)이 `expandrv.exe stop`을 호출해 미완료 업로드 대기와 드라이브 해제 결과를 직접 확인하도록 인계 순서를 보존합니다. 인스톨러 감시는 Tokio 런타임 종료를 막지 않는 독립 스레드에서 유한 대기 및 종료 플래그 연계로 처리되어 에이전트 종료 시 블로킹 교착을 방지하며, 인스톨러 실행 중 사용자가 취소하거나 비정상 종료되면 이전 준비 상태로 안전하게 복구합니다.

마운트 상태 변경, 업로드 완료와 충돌 사본 생성은 Windows 알림으로 표시합니다. GUI를 닫아도 에이전트가 알림을 유지합니다. 알림 표시는 Windows 알림 설정의 영향을 받습니다.

저장된 메타데이터가 있으면 원격 속성과 목록 확인은 최대 2초를 기다립니다. 네트워크 오류 또는 시간 초과 뒤에는 해당 드라이브의 원격 조회를 30초 동안 쉬며 저장된 목록, 속성과 파일 블록을 사용합니다. 캐시가 없는 항목은 네트워크 오류를 반환합니다. 대기 시간이 지나면 다음 조회에서 원격 연결을 다시 시도합니다. 원격 업로드는 기존 재시도 정책을 유지합니다.

## 드라이브 잠금

설정의 `드라이브 잠금`에서 숫자 6~12자리 PIN을 등록합니다. 상단 잠금 버튼 또는 트레이의 `잠금`으로 전체 마운트를 해제하고 남은 업로드를 정리합니다. 원격 업로드가 끝나지 않으면 변경분은 로컬 저널에 보존하며 다음 마운트에서 재개합니다. 백그라운드 원격 조회와 읽기 선행도 종료합니다.

잠긴 동안 마운트와 연결 확인을 차단합니다. PIN으로 잠금을 해제한 뒤 필요한 드라이브를 직접 마운트합니다. 에이전트는 마지막 잠금 상태를 기억합니다. PIN으로 해제했다면 다시 시작해도 해제 상태로 시작하고, 사용자가 잠그거나 유휴 자동 잠금이 걸린 상태였다면 잠긴 상태로 시작하며 자동 마운트 요청도 차단합니다. 상태 기록이 없거나 읽지 못하면 잠긴 상태로 시작합니다. PIN을 바꿀 때는 현재 PIN을 확인합니다. PIN을 연속 5회 틀리면 해당 에이전트에서 30초 동안 재입력을 기다립니다.

PIN 검증값은 설정 경로별로 분리하여 Windows 자격 증명 관리자에 DPAPI 암호화로 저장합니다. PIN 평문은 설정과 로그에 기록하지 않습니다. IPC 버전은 2이므로 기존 에이전트를 종료한 뒤 같은 빌드의 GUI와 CLI를 함께 사용하세요.

```powershell
expandrv pin       # PIN 등록 또는 변경, 화면에 표시하지 않고 입력
expandrv lock      # 전체 드라이브 잠금
expandrv unlock    # PIN 입력 후 잠금 해제
```

## 자동 잠금

PIN을 등록한 뒤 설정의 `자동 잠금`에서 Windows 잠금 연계와 유휴 시간을 선택할 수 있습니다. Windows 잠금 연계는 기본으로 켜져 있고, 유휴 자동 잠금은 `사용 안 함`이 기본값입니다. GUI를 닫아도 상주 에이전트가 감시합니다. 자동 잠금도 기존 업로드 정리와 저널 보존 절차를 사용합니다. Windows 잠금 연계로 해제한 드라이브는 Windows 잠금을 풀면 PIN 없이 다시 마운트하고, 유휴 자동 잠금은 PIN으로 직접 해제합니다.

유휴 시간은 에이전트가 실행 중인 Windows 세션의 마우스와 키보드 입력으로 판단합니다. PIN 해제 또는 정책 변경 뒤에는 선택한 시간이 새로 지나야 잠깁니다. Windows 세션 알림은 [WTSRegisterSessionNotification](https://learn.microsoft.com/en-us/windows/win32/api/wtsapi32/nf-wtsapi32-wtsregistersessionnotification), 유휴 시간은 [GetLastInputInfo](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getlastinputinfo)를 사용합니다.

```toml
[security]
lock_on_session_lock = true
idle_minutes = 15  # 0이면 사용 안 함, 1~1440분
```

설정 스키마는 v3입니다. v1과 v2 설정을 읽으면 `config.toml.v<기존 버전>.bak`을 보존한 뒤 자동 변환하며, 이미 있는 백업은 유지합니다. v2에는 적응형 선행 읽기 상한 기본값 4를 추가하고 기존 설정을 유지합니다. 새 실행 파일을 사용하기 전에 기존 에이전트를 종료하세요.

## 탐색기 우클릭 메뉴

설정의 `탐색기 메뉴`에서 켜거나 끌 수 있습니다. 현재 사용자의 드라이브, 폴더와 폴더 배경 메뉴에 `ExpanDrv 열기`와 `ExpanDrv에서 이 드라이브 관리`를 등록합니다. 관리자 권한은 필요하지 않습니다. Windows 11의 기존 확장 메뉴는 [추가 옵션 표시](https://blogs.windows.com/blog/2021/07/19/extending-the-context-menu-and-share-dialog-in-windows-11/)에서 사용할 수 있습니다.

드라이브 관리 메뉴는 선택한 경로를 포함하는 ExpanDrv 드라이브 속성을 엽니다. 중첩된 폴더 마운트에서는 가장 안쪽 드라이브를 선택하며, 일반 경로는 드라이브 목록을 엽니다. 이 과정에서 원격 파일을 조회하거나 자동 마운트하지 않습니다. PIN 잠금은 계속 적용합니다.

```powershell
expandrv shell enable     # 같은 폴더의 GUI를 탐색기 메뉴에 등록
expandrv shell disable    # ExpanDrv 메뉴만 해제
expandrv shell status     # 현재 등록 상태
```

등록은 현재 GUI와 설정 파일의 경로를 저장합니다. 실행 파일 위치를 바꾼 뒤에는 메뉴를 다시 등록하세요. CLI의 `--config`로 등록한 메뉴는 같은 설정 파일을 GUI에도 전달합니다.

## 개발

```powershell
pwsh -File scripts/check.ps1   # fmt, clippy -D warnings, test
```

- GUI 화면은 `gui`에서 `npm run lint`(ESLint, 형식 검사)와 `npm run build`로 확인합니다. `npm run tauri -- dev -- --no-default-features`로 Vite 개발 서버와 함께 띄웁니다.
- `cargo test`의 마운트 통합 테스트는 WinFsp 런타임이 있을 때만 실제 드라이브 문자로 실행됩니다.
- 네트워크 백엔드 통합 테스트는 `EXPANDRV_TEST_*` 환경 변수가 있을 때만 실행됩니다. 각 테스트 파일 머리 주석에 Docker로 서버를 띄우는 방법이 있습니다.
- 성능 변경 후 `cargo test -p expandrv-vfs performance_comparison --release -- --ignored --nocapture`로 헤더 조회 다운로드량과 업로드 중 폴더 조회 p95를 통제된 지연 백엔드에서 비교합니다. 원격 요청 수와 바이트 수, 지연을 출력하며 실제 네트워크 처리량과 구분합니다.
- 실제 WinFsp 마운트 측정은 릴리스 CLI 빌드 후 `pwsh -NoProfile -File scripts/bench.ps1 -SkipWebDav`로 실행합니다. 기본 1GiB 읽기, 1만 항목 열거와 유휴 메모리를 측정하고 `target/bench-latest.json`에 기록합니다. `-Executable <경로> -ResultPath <JSON 경로>`로 비교 실행 파일과 결과 경로를 지정할 수 있습니다. 이 스크립트가 `scripts/bench-agent.ps1`을 WMI로 호출해 별도 IPC와 임시 데이터 폴더의 에이전트를 시작하고 완료 후 종료합니다. 사용자 드라이브와 캐시는 그대로 유지하며 측정 중 디스크 블록 캐시는 끕니다. 실패하면 실행 로그와 측정 에이전트 종료 여부를 확인한 뒤 재실행하세요. `-SkipWebDav`를 생략하면 Docker의 rclone WebDAV도 측정하므로 Docker 실행 환경과 빈 포트 18095가 필요합니다.

| 크레이트 | 역할 |
|---|---|
| `expandrv-core` | 원격 경로, 오류 모델, `Backend` 트레이트, 설정 |
| `expandrv-backends` | Local, WebDAV, SFTP, S3, FTP 백엔드 |
| `expandrv-vfs` | 핸들 테이블, 메타와 블록 캐시, write-back 업로드, 변경 감지 |
| `expandrv-winfsp` | WinFsp 바인딩, 보안 서술자, 마운트 |
| `expandrv-secrets` | DPAPI와 Windows 자격 증명 관리자 비밀 값 저장소 |
| `expandrv-ipc` | 소유자 전용 이름 있는 파이프 IPC(GUI와 CLI가 에이전트를 제어) |
| `expandrv-log` | 로그 비밀 값과 개인정보 마스킹 |
| `expandrv-shell` | 현재 사용자 탐색기 메뉴 등록과 마운트 경로 식별 |
| `expandrv-autostart` | 로그인 자동 실행 등록(현재 사용자 Run 키) |
| `expandrv-agent` | `expandrv` CLI와 상주 에이전트 |
| `gui` | 데스크톱 GUI(Tauri 2, React). `gui/src-tauri`가 `expandrv-gui` 크레이트 |

## 라이선스

[GNU General Public License v3.0](LICENSE)(`GPL-3.0-only`)

- 파일 시스템 계층은 [WinFsp](https://github.com/winfsp/winfsp)(GPLv3, FLOSS 예외)와 Rust 바인딩 [winfsp-rs](https://github.com/SnowflakePowered/winfsp-rs)(GPL-3.0)를 사용합니다.
- 그 밖의 의존성은 MIT, Apache-2.0, BSD, ISC, Zlib, Unicode-3.0 등 GPL-3.0과 호환되는 라이선스입니다.
