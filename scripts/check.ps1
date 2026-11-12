# 워크스페이스 품질 게이트: 포맷, clippy(경고를 오류로), 테스트
# 사용법: pwsh -File scripts/check.ps1 [-Fix]
param([switch]$Fix)
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot -Parent)

function Step($name, [scriptblock]$cmd) {
    Write-Host "==> $name"
    & $cmd
    if ($LASTEXITCODE -ne 0) { Write-Host "FAIL: $name"; exit $LASTEXITCODE }
}

if ($Fix) { Step 'cargo fmt' { cargo fmt --all } }
else { Step 'cargo fmt --check' { cargo fmt --all -- --check } }
Step 'cargo clippy' { cargo clippy --workspace --all-targets --all-features -- -D warnings }
Step 'cargo test' { cargo test --workspace --all-features }
Step 'SSH 파일 도우미 테스트' { python -B crates/expandrv-backends/src/sftp/ssh_files_tests.py }
Write-Host 'OK: fmt, clippy, test 통과'
