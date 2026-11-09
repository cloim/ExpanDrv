# 실행: scripts/package.ps1 완료 후 pwsh -File scripts/test-installer.ps1
# 설치된 WinFsp 2.1 이상을 사용합니다. 별도 제품명, 설정과 IPC로 EXE 설치/업그레이드/제거를 확인합니다.
param()
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$projectRoot = Split-Path $PSScriptRoot -Parent
Set-Location -LiteralPath $projectRoot
$testRoot = Join-Path $projectRoot 'target/installer-smoke'
$product = 'ExpanDrv Installer Test'
$uninstallKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$product"
$preferencesKey = "HKCU:\Software\$product"
$shortcutDir = Join-Path ([Environment]::GetFolderPath('StartMenu')) "Programs/$product"
foreach ($path in @($uninstallKey, $preferencesKey, $shortcutDir)) {
    if (Test-Path -LiteralPath $path) { throw "테스트 전용 이름이 사용 중입니다: $path" }
}
$runtime = 'C:/Program Files (x86)/WinFsp/bin/winfsp-x64.dll'
if (-not (Test-Path -LiteralPath $runtime)) { throw 'WinFsp를 먼저 설치한 환경에서 실행하세요.' }
$runtimeInfo = (Get-Item -LiteralPath $runtime).VersionInfo
if ($runtimeInfo.FileMajorPart -lt 2 -or ($runtimeInfo.FileMajorPart -eq 2 -and $runtimeInfo.FileMinorPart -lt 1)) { throw 'WinFsp 2.1 이상이 필요합니다.' }
$savedEnvironment = @{}
foreach ($name in @('APPDATA', 'LOCALAPPDATA', 'TEMP', 'TMP', 'EXPANDRV_PIPE')) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$installDir = Join-Path $testRoot 'installed app'
if (Test-Path -LiteralPath $installDir) { throw '기존 테스트 설치 폴더를 확인한 뒤 새 경로로 실행하세요.' }
New-Item -ItemType Directory -Force -Path $testRoot | Out-Null
$testBinary = Join-Path $projectRoot 'target/release/expandrv-installer-test.exe'
Copy-Item -LiteralPath (Join-Path $projectRoot 'target/release/expandrv-gui.exe') -Destination $testBinary -Force
$configPath = Join-Path $testRoot 'test-bundle.json'
$sourceCli = Join-Path $projectRoot 'target/release/expandrv.exe'
$driveId = "installer-test-$([guid]::NewGuid().ToString('N'))"
$driveAdded = $false
$result = [ordered]@{ install = $false; upgrade = $false; uninstall = $false; dataPreserved = $false; uploadPreserved = $false }

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Invoke-Cli([string]$Executable, [string[]]$Arguments) {
    & $Executable @Arguments
    if ($LASTEXITCODE -ne 0) { throw "CLI 실패: $Arguments ($LASTEXITCODE)" }
}
function Build-TestInstaller([string]$Version) {
    @{
        productName = $product; mainBinaryName = 'expandrv-installer-test'; version = $Version
        identifier = 'com.example.expandrv.installer-test'
        bundle = @{ publisher = $product; windows = @{ nsis = @{ startMenuFolder = $product } } }
    } | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $configPath -Encoding utf8
    Push-Location (Join-Path $projectRoot 'gui')
    try {
        & npm.cmd run tauri -- bundle --ci --bundles nsis --no-sign --config src-tauri/tauri.release.conf.json --config $configPath | Out-Host
        if ($LASTEXITCODE -ne 0) { throw '테스트 설치 파일 생성 실패' }
    } finally { Pop-Location }
    $file = Join-Path $projectRoot "target/release/bundle/nsis/${product}_${Version}_x64-setup.exe"
    Assert-True (Test-Path -LiteralPath $file) "테스트 설치 파일 없음: $file"
    return $file
}
function Run-Installer([string]$Executable, [string]$Arguments) {
    $info = [Diagnostics.ProcessStartInfo]::new($Executable, $Arguments)
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $process = [Diagnostics.Process]::Start($info)
    if (-not $process.WaitForExit(180000)) { throw "설치 작업 시간 초과: $Executable" }
    Assert-True ($process.ExitCode -eq 0) "설치 종료 코드: $($process.ExitCode)"
    $process.Dispose()
}

try {
    # 번들러는 원래 도구 캐시를 사용하고 설치된 앱만 격리된 프로필을 사용합니다.
    $initialInstaller = Build-TestInstaller '0.1.0'
    $upgradeInstaller = Build-TestInstaller '0.1.1'
    foreach ($pair in @{ APPDATA = 'profile/roaming'; LOCALAPPDATA = 'profile/local'; TEMP = 'temp'; TMP = 'temp' }.GetEnumerator()) {
        $directory = Join-Path $testRoot $pair.Value
        New-Item -ItemType Directory -Force -Path $directory | Out-Null
        [Environment]::SetEnvironmentVariable($pair.Key, $directory, 'Process')
    }
    $env:EXPANDRV_PIPE = "\\.\pipe\expandrv-installer-$([guid]::NewGuid().ToString('N'))"
    Run-Installer $initialInstaller "/S /D=$installDir"
    foreach ($file in @('expandrv-installer-test.exe', 'expandrv.exe', 'uninstall.exe', 'LICENSE.txt', 'WinFsp-LICENSE.txt', 'THIRD-PARTY-NOTICES.txt')) {
        Assert-True (Test-Path -LiteralPath (Join-Path $installDir $file)) "설치 파일 누락: $file"
    }
    Assert-True (Test-Path -LiteralPath $uninstallKey) '제거 등록 없음'
    Assert-True (Test-Path -LiteralPath $shortcutDir) '시작 메뉴 없음'
    $result.install = $true
    $cli = Join-Path $installDir 'expandrv.exe'
    $remote = Join-Path $testRoot 'local-backend'
    New-Item -ItemType Directory -Force -Path $remote | Out-Null
    $occupied = @([IO.DriveInfo]::GetDrives() | ForEach-Object { $_.Name.Substring(0, 1) })
    $letter = @('Z','Y','X','W','V','U','T','S','R','Q','P','O','N','M','L','K','J','I','H') | Where-Object { $_ -notin $occupied } | Select-Object -First 1
    Assert-True ([bool]$letter) '빈 드라이브 문자 없음'
    Invoke-Cli $cli @('add', $driveId, "${letter}:", 'local', $remote)
    $driveAdded = $true
    Invoke-Cli $cli @('mount', $driveId)
    $payload = [byte[]]::new(1MB)
    [Security.Cryptography.RandomNumberGenerator]::Fill($payload)
    [IO.File]::WriteAllBytes("${letter}:\installer-upload.bin", $payload)
    $expectedHash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($payload))
    $profileConfig = Join-Path $env:APPDATA 'ExpanDrv/config.toml'
    $configHash = (Get-FileHash -LiteralPath $profileConfig).Hash
    $sentinel = Join-Path $env:LOCALAPPDATA 'ExpanDrv/preserved.txt'
    New-Item -ItemType Directory -Force -Path (Split-Path $sentinel -Parent) | Out-Null
    Set-Content -LiteralPath $sentinel -Value 'keep-after-uninstall' -Encoding utf8
    Run-Installer $upgradeInstaller "/S /D=$installDir"
    Assert-True (-not (Test-Path -LiteralPath "${letter}:\")) '업그레이드 후 마운트가 남았습니다'
    Assert-True ((Get-FileHash -LiteralPath (Join-Path $remote 'installer-upload.bin')).Hash -eq $expectedHash) '업그레이드 종료 중 업로드 손상'
    Assert-True ((Get-FileHash -LiteralPath $profileConfig).Hash -eq $configHash) '업그레이드 중 설정 변경'
    Assert-True ((Get-ItemProperty -LiteralPath $uninstallKey).DisplayVersion -eq '0.1.1') '업그레이드 버전 불일치'
    $result.upgrade = $true
    $result.uploadPreserved = $true
    Run-Installer (Join-Path $installDir 'uninstall.exe') '/S'
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    while (((Test-Path -LiteralPath (Join-Path $installDir 'expandrv.exe')) -or (Test-Path -LiteralPath $uninstallKey) -or (Test-Path -LiteralPath $shortcutDir)) -and [DateTime]::UtcNow -lt $deadline) { Start-Sleep -Milliseconds 200 }
    Assert-True (-not (Test-Path -LiteralPath (Join-Path $installDir 'expandrv.exe'))) '제거 후 실행 파일이 남았습니다'
    Assert-True (-not (Test-Path -LiteralPath $uninstallKey)) '제거 등록이 남았습니다'
    Assert-True (-not (Test-Path -LiteralPath $shortcutDir)) '시작 메뉴가 남았습니다'
    Assert-True ((Get-FileHash -LiteralPath $profileConfig).Hash -eq $configHash) '제거 중 설정 변경'
    Assert-True ((Get-Content -LiteralPath $sentinel -Raw).Trim() -eq 'keep-after-uninstall') '제거 중 사용자 데이터 삭제'
    Assert-True (Test-Path -LiteralPath $runtime) '공유 WinFsp 런타임 누락'
    $result.uninstall = $true
    $result.dataPreserved = $true
} finally {
    # 테스트 전용 파이프와 UUID에만 접근합니다.
    if ($env:EXPANDRV_PIPE -ne $savedEnvironment['EXPANDRV_PIPE']) {
        & $sourceCli stop
        if ($driveAdded) { & $sourceCli remove $driveId }
    }
    foreach ($name in $savedEnvironment.Keys) { [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process') }
    # 사전 조회에서 비어 있던 테스트 제품 전용 설치 환경설정만 정리합니다.
    if ($result.uninstall -and (Test-Path -LiteralPath $preferencesKey)) { Remove-Item -LiteralPath $preferencesKey -Recurse -Force }
    $result | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $testRoot 'result.json') -Encoding utf8
}
Write-Host '설치, 업그레이드, 업로드 보존, 제거와 사용자 데이터 보존 검증 완료'
