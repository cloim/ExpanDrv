# ExpanDrv

원격 스토리지(WebDAV, SFTP, FTP, S3 호환, 클라우드 드라이브)를 Windows 드라이브로 마운트하는 Rust 네이티브 앱입니다.

- 파일 시스템 계층: [WinFsp](https://winfsp.dev/)
- 목표: 사용자 세션 권한 분리, 자격 증명 암호화 저장, 낮은 메모리 사용과 높은 처리량

## 요구 사항

- Windows 10 이상 x64
- WinFsp 2.x 런타임
- Rust stable (MSVC 툴체인)
