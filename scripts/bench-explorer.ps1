# 탐색기 및 파일 형식별(텍스트, 이미지, 동영상, 문서) 아이콘, 썸네일, 미리보기 조회 성능 및 정합성 실측 스크립트
# 사용법: pwsh -NoProfile -File scripts/bench-explorer.ps1 [-ResultPath <JSON>]
param(
    [string]$Executable = "",
    [string]$ResultPath = "target/bench-explorer.json"
)

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8

$repo = Split-Path $PSScriptRoot -Parent
$exe = $Executable
if (-not $exe) {
    $exe = Join-Path $repo 'target\release\expandrv.exe'
    if (-not (Test-Path $exe)) {
        $exe = Join-Path $repo 'target\debug\expandrv.exe'
    }
}
if (-not (Test-Path $exe)) {
    throw "실행 파일이 없습니다. cargo build를 먼저 수행하세요."
}

# Win32 GetFileInformationByHandle P/Invoke
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

public class Win32FileId {
    [StructLayout(LayoutKind.Sequential)]
    public struct BY_HANDLE_FILE_INFORMATION {
        public uint FileAttributes;
        public System.Runtime.InteropServices.ComTypes.FILETIME CreationTime;
        public System.Runtime.InteropServices.ComTypes.FILETIME LastAccessTime;
        public System.Runtime.InteropServices.ComTypes.FILETIME LastWriteTime;
        public uint VolumeSerialNumber;
        public uint FileSizeHigh;
        public uint FileSizeLow;
        public uint NumberOfLinks;
        public uint FileIndexHigh;
        public uint FileIndexLow;
    }

    [DllImport("kernel32.dll", SetLastError = true)]
    public static extern bool GetFileInformationByHandle(SafeFileHandle hFile, out BY_HANDLE_FILE_INFORMATION lpFileInformation);

    public static ulong GetFileId(string path) {
        using (var fs = new System.IO.FileStream(path, System.IO.FileMode.Open, System.IO.FileAccess.Read, System.IO.FileShare.ReadWrite)) {
            BY_HANDLE_FILE_INFORMATION info;
            if (!GetFileInformationByHandle(fs.SafeFileHandle, out info)) {
                throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
            }
            return ((ulong)info.FileIndexHigh << 32) | (ulong)info.FileIndexLow;
        }
    }
}
"@ -ErrorAction SilentlyContinue

$work = Join-Path $env:TEMP ('expandrv-exp-bench-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
$remoteDir = Join-Path $work 'remote'
$localAppDir = Join-Path $work 'localappdata'
$cfg = Join-Path $work 'config.toml'
$pipe = '\\.\pipe\expandrv-exp-bench-' + [guid]::NewGuid().ToString('N')

function Free-Letter {
    [char[]](72..90) | Sort-Object -Descending | Where-Object { -not (Test-Path "${_}:\") } | Select-Object -First 1
}

$driveLetter = Free-Letter
if (-not $driveLetter) {
    throw "사용 가능한 드라이브 문자가 없습니다."
}
$mountPoint = "${driveLetter}:"

try {
    New-Item -ItemType Directory -Force $remoteDir, $localAppDir | Out-Null
    $env:LOCALAPPDATA = $localAppDir
    $env:EXPANDRV_PIPE = $pipe

    Write-Host "==> 대표 파일 형식 생성 (텍스트, 이미지, 동영상, 문서)"
    $files = @{
        'sample.txt' = 64KB
        'photo.jpg'   = 4MB
        'movie.mp4'   = 50MB
        'doc.docx'    = 10MB
    }

    # 파일 생성
    # 텍스트
    [IO.File]::WriteAllBytes("$remoteDir\sample.txt", (New-Object byte[] 64KB))
    # JPEG (SOI 0xFFD8, APP0, 더미)
    $jpgBytes = New-Object byte[] 4MB
    $jpgBytes[0] = 0xFF; $jpgBytes[1] = 0xD8; $jpgBytes[2] = 0xFF; $jpgBytes[3] = 0xE0
    [IO.File]::WriteAllBytes("$remoteDir\photo.jpg", $jpgBytes)
    # MP4 (ftyp box + 끝부분 moov 더미)
    $mp4Bytes = New-Object byte[] 50MB
    # ftyp header: size 0x0000001C, 'ftyp', 'mp42'
    $mp4Bytes[3] = 0x1C; $mp4Bytes[4] = 0x66; $mp4Bytes[5] = 0x74; $mp4Bytes[6] = 0x79; $mp4Bytes[7] = 0x70
    $mp4Bytes[8] = 0x6D; $mp4Bytes[9] = 0x70; $mp4Bytes[10] = 0x34; $mp4Bytes[11] = 0x32
    # 끝부분 moov: 마지막 64KB 시작에 0x00010000, 'moov'
    $endMoov = 50MB - 64KB
    $mp4Bytes[$endMoov] = 0x00; $mp4Bytes[$endMoov+1] = 0x01; $mp4Bytes[$endMoov+2] = 0x00; $mp4Bytes[$endMoov+3] = 0x00
    $mp4Bytes[$endMoov+4] = 0x6D; $mp4Bytes[$endMoov+5] = 0x6F; $mp4Bytes[$endMoov+6] = 0x6F; $mp4Bytes[$endMoov+7] = 0x76
    [IO.File]::WriteAllBytes("$remoteDir\movie.mp4", $mp4Bytes)
    # DOCX (ZIP PK 0x03, 0x04, 끝부분 Central Directory PK 0x01, 0x02)
    $docxBytes = New-Object byte[] 10MB
    $docxBytes[0] = 0x50; $docxBytes[1] = 0x4B; $docxBytes[2] = 0x03; $docxBytes[3] = 0x04
    $endCD = 10MB - 64KB
    $docxBytes[$endCD] = 0x50; $docxBytes[$endCD+1] = 0x4B; $docxBytes[$endCD+2] = 0x01; $docxBytes[$endCD+3] = 0x02
    [IO.File]::WriteAllBytes("$remoteDir\doc.docx", $docxBytes)

    Write-Host "==> 마운트 준비: $mountPoint"
    & $exe --config $cfg add testdrv $mountPoint local $remoteDir | Out-Null
    
    # 에이전트 백그라운드 시작
    $helper = Join-Path $repo 'scripts\bench-agent.ps1'
    $commandLine = 'pwsh.exe -NoProfile -File "{0}" -Executable "{1}" -Config "{2}" -DataDir "{3}" -Pipe "{4}" -LogDir "{5}"' -f $helper, $exe, $cfg, $localAppDir, $pipe, $work
    $launch = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = $commandLine; CurrentDirectory = $repo }
    if ($launch.ReturnValue -ne 0) { throw "에이전트 시작 실패: $($launch.ReturnValue)" }

    $ready = $false
    for ($i = 0; $i -lt 100; $i++) {
        if ([IO.Directory]::GetFiles('\\.\pipe\') -contains $pipe) { $ready = $true; break }
        Start-Sleep -Milliseconds 100
    }
    if (-not $ready) { throw '측정 에이전트 준비 시간 초과' }

    & $exe --config $cfg mount testdrv | Out-Null
    for ($i = 0; $i -lt 100 -and -not (Test-Path "$mountPoint\"); $i++) { Start-Sleep -Milliseconds 100 }
    if (-not (Test-Path "$mountPoint\")) { throw "$mountPoint 마운트 실패" }

    Write-Host "==> 실측 시작"
    $metrics = [ordered]@{}

    # 1. FileId (index_number) 정합성 확인
    $fileIds = [ordered]@{}
    foreach ($fn in $files.Keys) {
        $mountedFile = "$mountPoint\$fn"
        $fid = [Win32FileId]::GetFileId($mountedFile)
        $fileIds[$fn] = "0x{0:X16}" -f $fid
    }
    $metrics['FileId_할당_결과'] = $fileIds
    $uniqueIds = $fileIds.Values | Select-Object -Unique
    $metrics['FileId_고유성_보장'] = ($uniqueIds.Count -eq $files.Count) -and ($uniqueIds -notcontains "0x0000000000000000")

    # 2. 파일 형식별 읽기/조회 실측 함수
    function Measure-ExplorerOps([string]$basePath) {
        $res = [ordered]@{}
        foreach ($fn in $files.Keys) {
            $p = Join-Path $basePath $fn
            $sw = [Diagnostics.Stopwatch]::StartNew()
            # (1) 탐색기 메타데이터 / 속성 조회
            $item = Get-Item -LiteralPath $p
            $metaMs = $sw.Elapsed.TotalMilliseconds

            # (2) 헤더 조회 (앞부분 16KiB)
            $sw.Restart()
            $fs = [IO.File]::OpenRead($p)
            $buf = New-Object byte[] 16384
            $readBytes = $fs.Read($buf, 0, $buf.Length)
            $headerMs = $sw.Elapsed.TotalMilliseconds

            # (3) 파일 끝부분 탐색 조회 (동영상 moov 또는 ZIP Central Directory 모사)
            $sw.Restart()
            if ($item.Length -gt 65536) {
                $fs.Seek(-65536, [IO.SeekOrigin]::End) | Out-Null
                $endBuf = New-Object byte[] 65536
                $endReadBytes = $fs.Read($endBuf, 0, $endBuf.Length)
            }
            $fs.Dispose()
            $endSeekMs = $sw.Elapsed.TotalMilliseconds

            $res[$fn] = [ordered]@{
                '속성조회_ms'     = [math]::Round($metaMs, 3)
                '헤더조회_ms'     = [math]::Round($headerMs, 3)
                '끝부분탐색_ms'   = [math]::Round($endSeekMs, 3)
                '헤더읽기_바이트' = $readBytes
            }
        }
        return $res
    }

    Write-Host "--> Local 원본 직접 측정"
    $localOps = Measure-ExplorerOps $remoteDir
    $metrics['Local_원본_직접접근'] = $localOps

    Write-Host "--> 마운트 드라이브 Cold (초기) 측정"
    $coldOps = Measure-ExplorerOps $mountPoint
    $metrics['마운트드라이브_Cold_초기접근'] = $coldOps

    Write-Host "--> 마운트 드라이브 Warm (캐시 예열) 측정"
    $warmOps = Measure-ExplorerOps $mountPoint
    $metrics['마운트드라이브_Warm_예열접근'] = $warmOps

    # 요약 출력
    $metrics | ConvertTo-Json -Depth 5 | Write-Host

    $dir = Split-Path $ResultPath -Parent
    if ($dir -and -not (Test-Path $dir)) { New-Item -ItemType Directory -Force $dir | Out-Null }
    $metrics | ConvertTo-Json -Depth 5 | Set-Content $ResultPath -Encoding UTF8
    Write-Host "==> 실측 완료: $ResultPath 저장됨"
}
finally {
    Write-Host "==> 정리"
    try { & $exe --config $cfg stop 2>$null | Out-Null } catch {}
    Start-Sleep -Seconds 1
    if (Test-Path $work) {
        Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
    }
}
