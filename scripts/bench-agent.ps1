# bench.ps1이 WMI로 호출하는 격리된 측정 에이전트. 직접 실행하지 않습니다.
param(
    [Parameter(Mandatory)][string]$Executable,
    [Parameter(Mandatory)][string]$Config,
    [Parameter(Mandatory)][string]$DataDir,
    [Parameter(Mandatory)][string]$Pipe,
    [Parameter(Mandatory)][string]$LogDir
)
$ErrorActionPreference = 'Stop'
$env:LOCALAPPDATA = $DataDir
$env:EXPANDRV_PIPE = $Pipe
& $Executable --config $Config --pipe $Pipe agent 1> "$LogDir\mount.out" 2> "$LogDir\mount.err"
exit $LASTEXITCODE
