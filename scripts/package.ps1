# 실행: 프로젝트 루트에서 pwsh -File scripts/package.ps1 [-SkipBuild] [-CertificateThumbprint <SHA1>]
# WinFsp 원본을 해시 검증하고 GUI, CLI와 함께 NSIS EXE 및 관리 배포용 MSI를 만듭니다.
param(
    [switch]$SkipBuild,
    [ValidatePattern('^[0-9A-Fa-f]{40}$')][string]$CertificateThumbprint,
    [string]$TimestampUrl = 'http://timestamp.digicert.com'
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$projectRoot = Split-Path $PSScriptRoot -Parent
Set-Location -LiteralPath $projectRoot
$tauriRoot = Join-Path $projectRoot 'gui/src-tauri'
$dependencyDir = Join-Path $tauriRoot 'installer/deps'
$dependency = (Get-Content -LiteralPath (Join-Path $tauriRoot 'installer/dependencies.json') -Raw | ConvertFrom-Json).winfsp
$config = Get-Content -LiteralPath (Join-Path $tauriRoot 'tauri.conf.json') -Raw | ConvertFrom-Json

function Invoke-Checked([string]$Name, [scriptblock]$Action) {
    Write-Host "==> $Name"
    & $Action
    if ($LASTEXITCODE -ne 0) { throw "$Name 실패: 종료 코드 $LASTEXITCODE" }
}

New-Item -ItemType Directory -Force -Path $dependencyDir | Out-Null
$runtimeMsi = Join-Path $dependencyDir 'winfsp-runtime.msi'
if (-not (Test-Path -LiteralPath $runtimeMsi)) {
    Invoke-WebRequest -Uri $dependency.url -OutFile $runtimeMsi
}
$hash = (Get-FileHash -LiteralPath $runtimeMsi -Algorithm SHA256).Hash
if ($hash -ne $dependency.sha256) { throw 'WinFsp MSI 해시 불일치. 원본을 확인한 뒤 deps 파일을 다시 받으세요.' }
$runtimeSignature = Get-AuthenticodeSignature -LiteralPath $runtimeMsi
if ($runtimeSignature.Status -ne 'Valid') { throw "WinFsp MSI 전자서명 검증 실패: $($runtimeSignature.Status)" }
$license = Join-Path $dependencyDir 'WinFsp-LICENSE.txt'
if (-not (Test-Path -LiteralPath $license)) { Invoke-WebRequest -Uri $dependency.licenseUrl -OutFile $license }
Write-Host "WinFsp $($dependency.version): SHA-256와 배포자 전자서명 검증 완료"

if (-not $SkipBuild) {
    Push-Location (Join-Path $projectRoot 'gui')
    try { Invoke-Checked '프런트엔드 빌드' { npm.cmd run build } }
    finally { Pop-Location }
    Invoke-Checked 'Rust 릴리스 빌드' { cargo build --release --workspace --offline }
}
foreach ($file in @('expandrv.exe', 'expandrv-gui.exe')) {
    if (-not (Test-Path -LiteralPath (Join-Path $projectRoot "target/release/$file"))) { throw "릴리스 파일이 없습니다: $file" }
}

$bundleArguments = @('run', 'tauri', '--', 'bundle', '--ci', '--bundles', 'nsis,msi', '--config', 'src-tauri/tauri.release.conf.json')
if ($CertificateThumbprint) {
    $signTool = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if (-not $signTool) { throw 'signtool.exe를 PATH에 추가한 뒤 서명 빌드를 실행하세요.' }
    foreach ($file in @('expandrv.exe', 'expandrv-gui.exe')) {
        $binary = Join-Path $projectRoot "target/release/$file"
        Invoke-Checked "$file 코드 서명" { & $signTool.Source sign /sha1 $CertificateThumbprint /fd SHA256 /tr $TimestampUrl /td SHA256 $binary }
    }
    $signingConfig = Join-Path $projectRoot 'target/installer-signing.json'
    @{ bundle = @{ windows = @{ certificateThumbprint = $CertificateThumbprint; digestAlgorithm = 'sha256'; timestampUrl = $TimestampUrl; tsp = $true } } } |
        ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $signingConfig -Encoding utf8
    $bundleArguments += @('--config', $signingConfig)
} else {
    $bundleArguments += '--no-sign'
}
Push-Location (Join-Path $projectRoot 'gui')
try { Invoke-Checked '설치 패키지 생성' { & npm.cmd @bundleArguments } }
finally { Pop-Location }

$releaseDir = Join-Path $projectRoot 'release'
New-Item -ItemType Directory -Force -Path $releaseDir | Out-Null
$artifacts = @()
foreach ($kind in @('nsis', 'msi')) {
    $pattern = if ($kind -eq 'nsis') { "ExpanDrv_$($config.version)_*-setup.exe" } else { "ExpanDrv_$($config.version)_*.msi" }
    $files = @(Get-ChildItem -LiteralPath (Join-Path $projectRoot "target/release/bundle/$kind") -Filter $pattern -File)
    if ($files.Count -eq 0) { throw "설치 산출물이 없습니다: $kind" }
    foreach ($file in $files) {
        $destination = Join-Path $releaseDir $file.Name
        Copy-Item -LiteralPath $file.FullName -Destination $destination -Force
        $signature = Get-AuthenticodeSignature -LiteralPath $destination
        if ($CertificateThumbprint -and $signature.Status -ne 'Valid') { throw "설치 파일 서명 검증 실패: $($file.Name)" }
        $artifacts += [ordered]@{
            file = $file.Name
            bytes = $file.Length
            sha256 = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash
            signature = $signature.Status.ToString()
            winfsp = if ($kind -eq 'nsis') { 'embedded' } else { 'required-before-install' }
        }
    }
}
$manifest = [ordered]@{
    version = $config.version
    architecture = 'x64'
    winfspVersion = $dependency.version
    winfspSha256 = $dependency.sha256
    artifacts = $artifacts
}
$manifestPath = Join-Path $releaseDir "ExpanDrv_$($config.version)_manifest.json"
$manifest | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $manifestPath -Encoding utf8
Write-Host "완료: $manifestPath"
