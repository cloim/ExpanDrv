# bench.ps1과 bench-write.ps1이 WMI로 호출하는 격리된 측정 에이전트. 직접 실행하지 않습니다.
param(
    [Parameter(Mandatory)][string]$Executable,
    [Parameter(Mandatory)][string]$Config,
    [Parameter(Mandatory)][string]$DataDir,
    [Parameter(Mandatory)][string]$Pipe,
    [Parameter(Mandatory)][string]$LogDir,
    # 선택: EXPANDRV_LOG 필터(예: warn,expandrv_vfs=debug)
    [string]$LogFilter,
    # 선택: 로그를 표준 오류 대신 <DataDir>\ExpanDrv\logs에 바로 씁니다. 멈춤을 진단할 때 씁니다.
    [switch]$FileLog
)
$ErrorActionPreference = 'Stop'
$env:LOCALAPPDATA = $DataDir
$env:EXPANDRV_PIPE = $Pipe
if ($LogFilter) { $env:EXPANDRV_LOG = $LogFilter }
$agentArgs = @('--config', $Config, '--pipe', $Pipe, 'agent')
if ($FileLog) { $agentArgs += '--background' }
& $Executable @agentArgs 1> "$LogDir\mount.out" 2> "$LogDir\mount.err"
exit $LASTEXITCODE
