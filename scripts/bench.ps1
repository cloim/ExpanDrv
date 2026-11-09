# 성능 목표 측정: 유휴 메모리, 1GB 순차 읽기 처리량, 1만 항목 디렉터리 열거
# 사용법: pwsh -NoProfile -File scripts/bench.ps1 [-SizeMB 1024] [-Files 10000] [-SkipWebDav] [-Executable <비교 실행 파일>] [-ResultPath <JSON>]
# 필요: 릴리스 빌드(target\release\expandrv.exe), WinFsp 런타임, WebDAV 측정 시 Docker
param(
    [int]$SizeMB = 1024,
    [int]$Files = 10000,
    [switch]$SkipWebDav,
    [string]$Executable,
    [string]$ResultPath
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$repo = Split-Path $PSScriptRoot -Parent
$exe = if ($Executable) { (Resolve-Path -LiteralPath $Executable).Path } else { Join-Path $repo 'target\release\expandrv.exe' }
if (-not $ResultPath) { $ResultPath = Join-Path $repo 'target\bench-latest.json' }
if (-not (Test-Path $exe)) { throw "릴리스 빌드가 없습니다: cargo build --release -p expandrv-agent" }

$work = Join-Path $env:TEMP ('expandrv-bench-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
$cfg = Join-Path $work 'config.toml'
$container = 'expandrv-bench-webdav'
$results = [ordered]@{}
$mountProc = $null
$savedLocalAppData = $env:LOCALAPPDATA
$savedPipe = $env:EXPANDRV_PIPE

function Read-Throughput([string]$path) {
    $buf = New-Object byte[] (1MB)
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $fs = [IO.FileStream]::new($path, 'Open', 'Read', 'Read', 1MB, [IO.FileOptions]::SequentialScan)
    $total = 0L
    try { while (($n = $fs.Read($buf, 0, $buf.Length)) -gt 0) { $total += $n } } finally { $fs.Dispose() }
    $sw.Stop()
    [pscustomobject]@{ Bytes = $total; MBps = [math]::Round($total / 1MB / $sw.Elapsed.TotalSeconds, 1) }
}

function Enum-Ms([string]$dir) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $count = @([IO.Directory]::EnumerateFileSystemEntries($dir)).Count
    $sw.Stop()
    [pscustomobject]@{ Count = $count; Ms = [math]::Round($sw.Elapsed.TotalMilliseconds, 1) }
}

function Memory-MB([int]$procId) {
    $p = Get-Process -Id $procId
    $p.Refresh()
    [pscustomobject]@{
        WorkingSet = [math]::Round($p.WorkingSet64 / 1MB, 1)
        Private    = [math]::Round($p.PrivateMemorySize64 / 1MB, 1)
    }
}

function Free-Letters([int]$n) {
    [char[]](72..90) | Sort-Object -Descending | Where-Object { -not (Test-Path "${_}:\") } | Select-Object -First $n
}

try {
    New-Item -ItemType Directory -Force "$work\big", "$work\many", "$work\small", "$work\localappdata" | Out-Null
    $env:LOCALAPPDATA = "$work\localappdata"
    $env:EXPANDRV_PIPE = '\\.\pipe\expandrv-bench-' + [guid]::NewGuid().ToString('N')

    Write-Host "==> 준비: ${SizeMB}MB 파일, 항목 ${Files}개"
    $chunk = New-Object byte[] (4MB); (New-Object Random 42).NextBytes($chunk)
    $fs = [IO.File]::Create("$work\big\big.bin")
    try { for ($i = 0; $i -lt $SizeMB / 4; $i++) { $fs.Write($chunk, 0, $chunk.Length) } } finally { $fs.Dispose() }
    $sub = New-Item -ItemType Directory "$work\many\dir"
    for ($i = 0; $i -lt $Files; $i++) { [IO.File]::WriteAllBytes("$($sub.FullName)\f$i.txt", [byte[]]@(1)) }
    'x' | Set-Content "$work\small\a.txt"

    $letters = @(Free-Letters 3)
    if ($letters.Count -lt 3) { throw '빈 드라이브 문자가 3개 필요합니다' }
    $mBig, $mMany, $mThird = $letters | ForEach-Object { "${_}:" }

    & $exe --config $cfg add bigdrv $mBig local "$work\big" | Out-Null
    & $exe --config $cfg add manydrv $mMany local "$work\many" | Out-Null
    if ($SkipWebDav) {
        & $exe --config $cfg add smalldrv $mThird local "$work\small" | Out-Null
    } else {
        Write-Host '==> WebDAV 서버(rclone) 기동'
        docker run -d --rm --name $container -p 18095:8080 rclone/rclone serve webdav /srv --addr :8080 | Out-Null
        docker exec $container sh -c "mkdir -p /srv && dd if=/dev/zero of=/srv/big.bin bs=4M count=$($SizeMB / 4) 2>/dev/null" | Out-Null
        for ($i = 0; $i -lt 50; $i++) {
            try { Invoke-WebRequest 'http://127.0.0.1:18095/' -Method Head -TimeoutSec 1 | Out-Null; break } catch { Start-Sleep -Milliseconds 200 }
        }
        & $exe --config $cfg add davdrv $mThird webdav 'http://127.0.0.1:18095/' | Out-Null
    }

    # 실제 사용자 캐시 키를 만들지 않도록 격리된 측정에서는 디스크 캐시를 끕니다.
    $configText = [IO.File]::ReadAllText($cfg).Replace('disk_limit_mib = 1024', 'disk_limit_mib = 0')
    [IO.File]::WriteAllText($cfg, $configText, [Text.UTF8Encoding]::new($false))
    Write-Host '==> 격리된 에이전트와 마운트'
    $helper = Join-Path $PSScriptRoot 'bench-agent.ps1'
    $commandLine = 'pwsh.exe -NoProfile -File "{0}" -Executable "{1}" -Config "{2}" -DataDir "{3}\localappdata" -Pipe "{4}" -LogDir "{3}"' -f $helper, $exe, $cfg, $work, $env:EXPANDRV_PIPE
    $launch = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $commandLine; CurrentDirectory = $repo }
    if ($launch.ReturnValue -ne 0) { throw "에이전트 시작 실패: $($launch.ReturnValue)" }
    $ready = $false
    for ($i = 0; $i -lt 100; $i++) {
        if ([IO.Directory]::GetFiles('\\.\pipe\') -contains $env:EXPANDRV_PIPE) { $ready = $true; break }
        Start-Sleep -Milliseconds 100
    }
    if (-not $ready) { throw '측정 에이전트 IPC 준비 시간을 초과했습니다' }
    & $exe --config $cfg mount | Out-Null
    if ($LASTEXITCODE -ne 0) { throw '측정 드라이브 마운트 실패' }
    $agent = Get-CimInstance Win32_Process -Filter "Name='expandrv.exe'" | Where-Object { $_.CommandLine.Contains($cfg) -and $_.CommandLine.Contains($env:EXPANDRV_PIPE) -and $_.CommandLine -match '\sagent(\s|$)' } | Select-Object -First 1
    if (-not $agent) { throw '측정 에이전트 프로세스를 찾지 못했습니다' }
    $mountProc = Get-Process -Id $agent.ProcessId
    foreach ($m in $mBig, $mMany, $mThird) {
        for ($i = 0; $i -lt 100 -and -not (Test-Path "$m\"); $i++) { Start-Sleep -Milliseconds 100 }
        if (-not (Test-Path "$m\")) { throw "$m 마운트 실패: $(Get-Content -Raw "$work\mount.err")" }
    }

    Start-Sleep -Seconds 5
    $idle = Memory-MB $mountProc.Id
    $results['유휴 메모리(작업 집합, 드라이브 3개)'] = "$($idle.WorkingSet) MB (전용 $($idle.Private) MB)"
    $results['유휴 메모리 목표 30MB 이하'] = if ($idle.WorkingSet -le 30) { '통과' } else { '미달' }

    Write-Host '==> 1GB 순차 읽기: Local'
    Read-Throughput "$work\big\big.bin" | Out-Null          # OS 캐시 예열
    $raw = Read-Throughput "$work\big\big.bin"
    $viaLocal = Read-Throughput "$mBig\big.bin"
    $ratioLocal = [math]::Round($viaLocal.MBps / $raw.MBps * 100, 1)
    $results['Local 원본 읽기'] = "$($raw.MBps) MB/s"
    $results['Local 드라이브 경유 읽기'] = "$($viaLocal.MBps) MB/s ($ratioLocal%)"

    if (-not $SkipWebDav) {
        Write-Host '==> 1GB 순차 읽기: WebDAV'
        $sw = [Diagnostics.Stopwatch]::StartNew()
        curl.exe -s -o NUL 'http://127.0.0.1:18095/big.bin'
        $sw.Stop()
        $davRaw = [math]::Round($SizeMB / $sw.Elapsed.TotalSeconds, 1)
        $viaDav = Read-Throughput "$mThird\big.bin"
        $ratioDav = [math]::Round($viaDav.MBps / $davRaw * 100, 1)
        $results['WebDAV 직접 HTTP 읽기(curl)'] = "$davRaw MB/s"
        $results['WebDAV 드라이브 경유 읽기'] = "$($viaDav.MBps) MB/s ($ratioDav%)"
        $results['처리량 목표 90% 이상(WebDAV)'] = if ($ratioDav -ge 90) { '통과' } else { '미달' }
    }

    Write-Host "==> 항목 ${Files}개 디렉터리 열거"
    $cold = Enum-Ms "$mMany\dir"
    $warm = 1..5 | ForEach-Object { (Enum-Ms "$mMany\dir").Ms } | Sort-Object
    $median = $warm[2]
    $results['열거 첫 회(원격 목록 조회)'] = "$($cold.Ms) ms ($($cold.Count)개)"
    $results['열거 캐시 상태(5회 중앙값)'] = "$median ms (최소 $($warm[0]) ms)"
    $results['열거 목표 100ms 이하'] = if ($median -le 100) { '통과' } else { '미달' }

    $after = Memory-MB $mountProc.Id
    $results['부하 직후 메모리(작업 집합)'] = "$($after.WorkingSet) MB (전용 $($after.Private) MB)"
    Start-Sleep -Seconds 40
    $rest = Memory-MB $mountProc.Id
    $results['부하 후 40초 유휴 메모리'] = "$($rest.WorkingSet) MB (전용 $($rest.Private) MB)"
}
finally {
    if ($launch -and $launch.ReturnValue -eq 0 -and -not $mountProc) {
        & $exe --config $cfg stop 2>$null | Out-Null
    }
    if ($mountProc -and -not $mountProc.HasExited) {
        & $exe --config $cfg stop 2>$null | Out-Null
        if (-not $mountProc.WaitForExit(30000)) { throw "측정 에이전트 종료를 확인하세요: PID $($mountProc.Id), $work" }
    }
    if (-not $SkipWebDav) { docker stop $container 2>$null | Out-Null }
    $env:LOCALAPPDATA = $savedLocalAppData
    $env:EXPANDRV_PIPE = $savedPipe
    $resolvedWork = [IO.Path]::GetFullPath($work)
    $tempRoot = [IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
    if (-not $resolvedWork.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -or (Split-Path $resolvedWork -Leaf) -notlike 'expandrv-bench-*') { throw '측정 임시 경로 확인 실패' }
    Remove-Item -LiteralPath $resolvedWork -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ''
Write-Host '==> 결과'
$results.GetEnumerator() | ForEach-Object { '{0,-36} {1}' -f $_.Key, $_.Value }
$results | ConvertTo-Json | Set-Content -LiteralPath $ResultPath -Encoding utf8
