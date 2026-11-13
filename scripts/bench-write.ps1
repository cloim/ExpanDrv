# 쓰기 일관성과 성능 측정: 드라이브로 복사, 이동했을 때 완료가 반환된 시점의 원격 상태와 원격 안착 시간을
# 실시간 업로드(through)와 백그라운드 업로드(back) 드라이브에서 대조합니다.
# 사용법:
#   Docker WebDAV(기본): pwsh -NoProfile -File scripts/bench-write.ps1 [-BigMB 256] [-SmallFiles 200] [-SmallKB 16]
#   실제 NAS:            pwsh -NoProfile -File scripts/bench-write.ps1 -NasDrive <설정의 드라이브 ID> -NasSsh <SSH 별칭> [-NasFtpBase /home] [-NasDiskBase <절대 경로>]
#   공통 선택:           [-Executable <실행 파일>] [-ResultPath <JSON>] [-LogFilter <EXPANDRV_LOG>]
# NAS 모드는 사용자 설정의 해당 드라이브(연결, 인증서 고정, 자격 증명 참조)를 복사해 격리된 측정 드라이브 두 개를 만들고,
# NAS의 측정 전용 폴더에만 씁니다. 원격 도착 여부는 측정 경로와 분리된 SSH로 확인하고, 끝나면 그 폴더만 지웁니다.
# 사용자 설정 파일과 기존 드라이브는 바꾸지 않습니다. 비밀 값은 읽거나 출력하지 않습니다.
# 필요: 릴리스 빌드(target\release\expandrv.exe), WinFsp 런타임, Docker(rclone/rclone, 기본 모드) 또는 SSH 키 인증(NAS 모드)
# 복사는 [IO.File]::Copy(CopyFileW)로 수행합니다. 탐색기 셸 복사 엔진과 같은 커널 요청을 쓰지만 진행 창은 재지 않습니다.
param(
    [int]$BigMB = 256,
    [int]$SmallFiles = 200,
    [int]$SmallKB = 16,
    [string]$Executable,
    [string]$ResultPath,
    # 측정 에이전트 로그 필터. 로그는 결과 파일 옆 *-agent.log로 남깁니다.
    [string]$LogFilter = 'info',
    # NAS 모드: 사용자 설정의 드라이브 ID와 원격 확인용 SSH 별칭
    [string]$NasDrive,
    [string]$NasSsh,
    # NAS 모드: 측정 폴더를 만들 드라이브 경로 기준과, SSH에서 본 같은 폴더의 절대 경로(기본: 원격 홈)
    [string]$NasFtpBase = '/home',
    [string]$NasDiskBase,
    [string]$UserConfig = (Join-Path $HOME '.config\ExpanDrv\config.toml')
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$repo = Split-Path $PSScriptRoot -Parent
$exe = if ($Executable) { (Resolve-Path -LiteralPath $Executable).Path } else { Join-Path $repo 'target\release\expandrv.exe' }
$nas = [bool]$NasDrive
if ($nas -and -not $NasSsh) { throw 'NAS 모드에는 -NasSsh가 필요합니다' }
if (-not $ResultPath) { $ResultPath = Join-Path $repo ($(if ($nas) { "target\bench-write-$NasDrive.json" } else { 'target\bench-write.json' })) }
if (-not (Test-Path $exe)) { throw "릴리스 빌드가 없습니다: cargo build --release -p expandrv-agent" }

$tag = [guid]::NewGuid().ToString('N').Substring(0, 8)
$work = Join-Path $env:TEMP "expandrv-bench-$tag"
$cfg = Join-Path $work 'config.toml'
$container = 'expandrv-bench-write'
$port = 18097
$benchDir = "expandrv-bench-$tag"
$remoteRoot = $null
$results = [ordered]@{}
$mountProc = $null
$launch = $null
$savedLocalAppData = $env:LOCALAPPDATA
$savedPipe = $env:EXPANDRV_PIPE

function Free-Letters([int]$n) {
    [char[]](72..90) | Sort-Object -Descending | Where-Object { -not (Test-Path "${_}:\") } | Select-Object -First $n
}

# 원격에서 셸 명령을 실행합니다(Docker 컨테이너 또는 NAS SSH).
function Remote-Sh([string]$command) {
    if ($nas) { ssh -o BatchMode=yes $NasSsh $command } else { docker exec $container sh -c $command }
}

# 원격 파일 크기. 없으면 -1
function Remote-Size([string]$rel) {
    [long](Remote-Sh "stat -c %s '$remoteRoot/$rel' 2>/dev/null || echo -1" | Select-Object -Last 1)
}

# 원격 폴더에서 이름 형식과 크기가 모두 맞는 완성 파일 수
function Remote-Complete([string]$dir, [long]$size) {
    [int](Remote-Sh "find '$remoteRoot/$dir' -maxdepth 1 -type f -name 'f*.bin' -size ${size}c 2>/dev/null | wc -l" | Select-Object -Last 1)
}

# 조건이 참이 될 때까지 기다리고 시작 시점부터의 경과(ms)를 돌려줍니다. 시간 초과면 -1
function Wait-Landed([Diagnostics.Stopwatch]$sw, [scriptblock]$done, [int]$timeoutSec = 600) {
    $deadline = [DateTime]::UtcNow.AddSeconds($timeoutSec)
    while (-not (& $done)) {
        if ([DateTime]::UtcNow -gt $deadline) { return -1 }
        Start-Sleep -Milliseconds 100
    }
    [math]::Round($sw.Elapsed.TotalMilliseconds, 0)
}

# 에이전트 목록에서 드라이브 상태 줄
function Drive-State([string]$id) {
    $line = & $exe --config $cfg --pipe $env:EXPANDRV_PIPE list | Where-Object { $_ -match "^$id\s" } | Select-Object -First 1
    if ($line) { ($line -replace '\s+', ' ').Trim() } else { '-' }
}

# 사용자 설정에서 드라이브 표 하나를 복사해 측정용으로 바꿉니다. 자격 증명 참조는 그대로 둡니다.
function Nas-DriveBlock([string]$source, [string]$id, [string]$mount, [string]$root, [string]$mode) {
    $m = [regex]::Match($source, '(?ms)^\[\[drive\]\]\s*\r?\nid = "' + [regex]::Escape($NasDrive) + '"\s*\r?\n.*?(?=^\[\[drive\]\]|^\[(?!drive)|\z)')
    if (-not $m.Success) { throw "사용자 설정에 드라이브 $NasDrive 가 없습니다" }
    $b = $m.Value -replace '(?m)^write_mode = ".*"\r?\n', ''
    $b = $b -replace '(?m)^id = ".*"', "id = `"$id`"`nwrite_mode = `"$mode`""
    $b = $b -replace '(?m)^label = ".*"', "label = `"bench $mode`""
    $b = $b -replace '(?m)^mount_point = ".*"', "mount_point = `"$mount`""
    $b = $b -replace '(?m)^auto_mount = .*', 'auto_mount = false'
    $b = $b -replace '(?m)^disk_limit_mib = .*', 'disk_limit_mib = 0'
    $b = $b -replace '(?m)^root = ".*"', "root = `"$root`""
    if ($b -notmatch [regex]::Escape("root = `"$root`"")) { throw '측정 드라이브의 원격 루트를 바꾸지 못했습니다' }
    $b
}

try {
    New-Item -ItemType Directory -Force "$work\src\many", "$work\localappdata" | Out-Null
    $env:LOCALAPPDATA = "$work\localappdata"
    $env:EXPANDRV_PIPE = '\\.\pipe\expandrv-bench-' + [guid]::NewGuid().ToString('N')

    Write-Host "==> 준비: ${BigMB}MB 파일 1개, ${SmallKB}KB 파일 ${SmallFiles}개, 이동용 32MB 파일"
    $rand = New-Object Random 42
    $chunk = New-Object byte[] (4MB); $rand.NextBytes($chunk)
    $fs = [IO.File]::Create("$work\src\big.bin")
    try { for ($i = 0; $i -lt $BigMB / 4; $i++) { $fs.Write($chunk, 0, $chunk.Length) } } finally { $fs.Dispose() }
    $small = New-Object byte[] ($SmallKB * 1KB); $rand.NextBytes($small)
    for ($i = 0; $i -lt $SmallFiles; $i++) { [IO.File]::WriteAllBytes("$work\src\many\f$i.bin", $small) }
    $moveBytes = New-Object byte[] (32MB); $rand.NextBytes($moveBytes)
    $bigBytes = (Get-Item "$work\src\big.bin").Length
    # 원본을 OS 캐시에 올려 두 모드가 같은 조건에서 읽게 합니다.
    [IO.File]::ReadAllBytes("$work\src\big.bin") | Out-Null

    $letters = @(Free-Letters 2)
    if ($letters.Count -lt 2) { throw '빈 드라이브 문자가 2개 필요합니다' }
    $drives = [ordered]@{ through = "$($letters[0]):"; back = "$($letters[1]):" }
    $ids = @{ through = 'thrdrv'; back = 'backdrv' }

    if ($nas) {
        Write-Host "==> NAS 측정 폴더 준비($NasSsh)"
        if (-not $NasDiskBase) { $NasDiskBase = (ssh -o BatchMode=yes $NasSsh 'cd ~ && pwd -P' | Select-Object -Last 1).Trim() }
        if ($NasDiskBase -notmatch '^/[^\s'']+$') { throw "NAS 기준 경로가 올바르지 않습니다: $NasDiskBase" }
        $remoteRoot = "$NasDiskBase/$benchDir"
        Remote-Sh "mkdir -p '$remoteRoot/through/many' '$remoteRoot/back/many' && echo ok > '$remoteRoot/through/marker.txt'" | Out-Null
        # 해석 기준: SSH 왕복(명령 하나 실행) 시간
        $sw = [Diagnostics.Stopwatch]::StartNew()
        1..5 | ForEach-Object { Remote-Sh 'true' | Out-Null }
        $results['기준: SSH 명령 왕복(접속 포함)'] = "$([math]::Round($sw.Elapsed.TotalMilliseconds / 5, 0)) ms/회"
        $source = [IO.File]::ReadAllText($UserConfig)
        $version = [regex]::Match($source, '(?m)^version = \d+').Value
        if (-not $version) { $version = 'version = 0' }
        $blocks = foreach ($mode in $drives.Keys) {
            Nas-DriveBlock $source $ids[$mode] $drives[$mode] "$NasFtpBase/$benchDir/$mode" $mode
        }
        [IO.File]::WriteAllText($cfg, ($version + "`n`n" + ($blocks -join "`n")), [Text.UTF8Encoding]::new($false))
    } else {
        Write-Host '==> WebDAV 서버(rclone) 기동'
        $remoteRoot = '/srv'
        docker run -d --rm --name $container -p "${port}:8080" rclone/rclone serve webdav /srv --addr :8080 | Out-Null
        Remote-Sh 'mkdir -p /srv/through/many /srv/back/many' | Out-Null
        for ($i = 0; $i -lt 50; $i++) {
            try { Invoke-WebRequest "http://127.0.0.1:${port}/" -Method Head -TimeoutSec 1 | Out-Null; break } catch { Start-Sleep -Milliseconds 200 }
        }
        # 해석 기준: 같은 서버에 curl로 작은 파일을 연결 재사용 PUT 했을 때의 회당 시간(드라이브를 거치지 않음)
        $probe = "$work\src\many\f0.bin"
        $putArgs = 1..20 | ForEach-Object { '-T', $probe, "http://127.0.0.1:${port}/probe-$_" }
        $sw = [Diagnostics.Stopwatch]::StartNew()
        curl.exe -s -o NUL @putArgs
        $results['서버 기준: 작은 파일 PUT(curl, 연결 재사용)'] = "$([math]::Round($sw.Elapsed.TotalMilliseconds / 20, 1)) ms/회"
        Remote-Sh 'rm -f /srv/probe-*' | Out-Null
        & $exe --config $cfg add $ids.through $drives.through webdav "http://127.0.0.1:${port}/through/" | Out-Null
        & $exe --config $cfg add $ids.back $drives.back webdav "http://127.0.0.1:${port}/back/" | Out-Null
        # 디스크 캐시를 끄고(실제 사용자 캐시 키를 만들지 않음) 두 번째 드라이브만 백그라운드 업로드로 둡니다.
        $text = [IO.File]::ReadAllText($cfg).Replace('disk_limit_mib = 1024', 'disk_limit_mib = 0')
        $text = [regex]::Replace($text, '(id = "backdrv"[\s\S]*?)write_mode = "through"', '$1write_mode = "back"')
        if ($text -notmatch 'write_mode = "back"') { throw '백그라운드 드라이브 설정을 바꾸지 못했습니다' }
        [IO.File]::WriteAllText($cfg, $text, [Text.UTF8Encoding]::new($false))
    }

    Write-Host '==> 격리된 에이전트와 마운트'
    $helper = Join-Path $PSScriptRoot 'bench-agent.ps1'
    $commandLine = 'pwsh.exe -NoProfile -File "{0}" -Executable "{1}" -Config "{2}" -DataDir "{3}\localappdata" -Pipe "{4}" -LogDir "{3}" -LogFilter "{5}" -FileLog' -f $helper, $exe, $cfg, $work, $env:EXPANDRV_PIPE, $LogFilter
    $launch = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $commandLine; CurrentDirectory = $repo }
    if ($launch.ReturnValue -ne 0) { throw "에이전트 시작 실패: $($launch.ReturnValue)" }
    $ready = $false
    for ($i = 0; $i -lt 100; $i++) {
        if ([IO.Directory]::GetFiles('\\.\pipe\') -contains $env:EXPANDRV_PIPE) { $ready = $true; break }
        Start-Sleep -Milliseconds 100
    }
    if (-not $ready) { throw '측정 에이전트 IPC 준비 시간을 초과했습니다' }
    $mountOut = & $exe --config $cfg --pipe $env:EXPANDRV_PIPE mount 2>&1
    if ($LASTEXITCODE -ne 0) { throw "측정 드라이브 마운트 실패: $mountOut" }
    $agent = Get-CimInstance Win32_Process -Filter "Name='expandrv.exe'" | Where-Object { $_.CommandLine.Contains($cfg) -and $_.CommandLine.Contains($env:EXPANDRV_PIPE) -and $_.CommandLine -match '\sagent(\s|$)' } | Select-Object -First 1
    if (-not $agent) { throw '측정 에이전트 프로세스를 찾지 못했습니다' }
    $mountProc = Get-Process -Id $agent.ProcessId
    foreach ($m in $drives.Values) {
        for ($i = 0; $i -lt 100 -and -not (Test-Path "$m\"); $i++) { Start-Sleep -Milliseconds 100 }
        if (-not (Test-Path "$m\")) { throw "$m 마운트 실패: $(Get-Content -Raw "$work\mount.err")" }
    }
    if ($nas) {
        # 드라이브 경로와 SSH 경로가 같은 폴더인지, 마운트 전에 SSH로 만든 표식 파일로 확인합니다.
        if (-not (Test-Path "$($drives.through)\marker.txt")) { throw "드라이브 경로($NasFtpBase)와 SSH 경로($NasDiskBase)가 서로 다른 폴더입니다" }
    }

    foreach ($mode in $drives.Keys) {
        $m = $drives[$mode]
        $id = $ids[$mode]
        $label = if ($mode -eq 'through') { '실시간' } else { '백그라운드' }

        Write-Host "==> [$label] ${BigMB}MB 파일 복사"
        $sw = [Diagnostics.Stopwatch]::StartNew()
        [IO.File]::Copy("$work\src\big.bin", "$m\big.bin", $true)
        $copyMs = [math]::Round($sw.Elapsed.TotalMilliseconds, 0)
        $atReturn = Remote-Size "$mode/big.bin"
        $state = Drive-State $id
        $landed = if ($atReturn -eq $bigBytes) { $copyMs } else { Wait-Landed $sw { (Remote-Size "$mode/big.bin") -eq $bigBytes } }
        $results["[$label] 큰 파일 복사 반환"] = "$copyMs ms"
        $results["[$label] 큰 파일 반환 시점 원격"] = if ($atReturn -eq $bigBytes) { '완료' } else { "미완료($atReturn / $bigBytes 바이트)" }
        $results["[$label] 큰 파일 반환 시점 에이전트"] = $state
        $results["[$label] 큰 파일 원격 안착"] = "$landed ms"
        if ($landed -gt 0) { $results["[$label] 큰 파일 실효 처리량"] = "$([math]::Round($bigBytes / 1MB / ($landed / 1000), 1)) MB/s" }

        Write-Host "==> [$label] ${SmallKB}KB 파일 ${SmallFiles}개 복사"
        $sw = [Diagnostics.Stopwatch]::StartNew()
        for ($i = 0; $i -lt $SmallFiles; $i++) { [IO.File]::Copy("$work\src\many\f$i.bin", "$m\many\f$i.bin", $true) }
        $copyMs = [math]::Round($sw.Elapsed.TotalMilliseconds, 0)
        $atReturn = Remote-Complete "$mode/many" ($SmallKB * 1KB)
        $state = Drive-State $id
        $landed = if ($atReturn -eq $SmallFiles) { $copyMs } else { Wait-Landed $sw { (Remote-Complete "$mode/many" ($SmallKB * 1KB)) -eq $SmallFiles } }
        $results["[$label] 작은 파일 복사 반환"] = "$copyMs ms ($([math]::Round($copyMs / $SmallFiles, 1)) ms/개)"
        $results["[$label] 작은 파일 반환 시점 원격"] = "$atReturn / $SmallFiles 개 완료"
        $results["[$label] 작은 파일 반환 시점 에이전트"] = $state
        $results["[$label] 작은 파일 원격 안착"] = "$landed ms"

        Write-Host "==> [$label] 32MB 파일 이동"
        [IO.File]::WriteAllBytes("$work\src\move.bin", $moveBytes)
        $sw = [Diagnostics.Stopwatch]::StartNew()
        Move-Item -LiteralPath "$work\src\move.bin" -Destination "$m\move.bin"
        $moveMs = [math]::Round($sw.Elapsed.TotalMilliseconds, 0)
        $sourceGone = -not (Test-Path "$work\src\move.bin")
        $atReturn = Remote-Size "$mode/move.bin"
        $landed = if ($atReturn -eq $moveBytes.Length) { $moveMs } else { Wait-Landed $sw { (Remote-Size "$mode/move.bin") -eq $moveBytes.Length } }
        $results["[$label] 이동 반환"] = "$moveMs ms"
        $results["[$label] 이동 반환 시점"] = "원본 삭제 $(if ($sourceGone) { '예' } else { '아니오' }), 원격 $(if ($atReturn -eq $moveBytes.Length) { '완료' } else { "미완료($atReturn 바이트)" })"
        $results["[$label] 이동 원격 안착"] = "$landed ms"

        Write-Host "==> [$label] 내용 대조"
        $remoteHash = (Remote-Sh "sha256sum '$remoteRoot/$mode/big.bin'" | Select-Object -Last 1).Split(' ')[0]
        $localHash = (Get-FileHash "$work\src\big.bin" -Algorithm SHA256).Hash.ToLowerInvariant()
        $results["[$label] 큰 파일 원격 해시 일치"] = if ($remoteHash -eq $localHash) { '예' } else { "아니오($remoteHash)" }
    }
}
finally {
    if ($launch -and $launch.ReturnValue -eq 0 -and -not $mountProc) {
        & $exe --config $cfg --pipe $env:EXPANDRV_PIPE stop 2>$null | Out-Null
    }
    if ($mountProc -and -not $mountProc.HasExited) {
        & $exe --config $cfg --pipe $env:EXPANDRV_PIPE stop 2>$null | Out-Null
        if (-not $mountProc.WaitForExit(60000)) { throw "측정 에이전트 종료를 확인하세요: PID $($mountProc.Id), $work" }
    }
    if ($nas) {
        # 이번 측정이 만든 폴더 하나만 지웁니다.
        if ($remoteRoot -and $remoteRoot -match "/$([regex]::Escape($benchDir))$") {
            ssh -o BatchMode=yes $NasSsh "rm -rf '$remoteRoot'" 2>$null | Out-Null
        }
    } else {
        docker stop $container 2>$null | Out-Null
    }
    # 에이전트는 로그를 격리된 데이터 폴더에 바로 씁니다. 표준 오류(시작 실패 등)를 뒤에 붙여 함께 남깁니다.
    $agentLog = [IO.Path]::ChangeExtension($ResultPath, $null).TrimEnd('.') + '-agent.log'
    $logs = @(Get-ChildItem -Path "$work\localappdata\ExpanDrv\logs" -Filter 'agent*.log' -ErrorAction SilentlyContinue)
    $parts = @($logs | ForEach-Object { [IO.File]::ReadAllText($_.FullName) })
    if (Test-Path "$work\mount.err") { $parts += [IO.File]::ReadAllText("$work\mount.err") }
    if ($parts.Count) { [IO.File]::WriteAllText($agentLog, ($parts -join "`n"), [Text.UTF8Encoding]::new($false)) }
    $env:LOCALAPPDATA = $savedLocalAppData
    $env:EXPANDRV_PIPE = $savedPipe
    $resolvedWork = [IO.Path]::GetFullPath($work)
    $tempRoot = [IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
    if (-not $resolvedWork.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -or (Split-Path $resolvedWork -Leaf) -notlike 'expandrv-bench-*') { throw '측정 임시 경로 확인 실패' }
    Remove-Item -LiteralPath $resolvedWork -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ''
Write-Host '==> 결과'
$results.GetEnumerator() | ForEach-Object { '{0,-34} {1}' -f $_.Key, $_.Value }
$results | ConvertTo-Json | Set-Content -LiteralPath $ResultPath -Encoding utf8
