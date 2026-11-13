# LSW Tools installer: installs or updates the LSW guest agent and WinFsp.
# Run by firstboot.ps1 during setup, and by the agent itself when the LSW
# Tools CD carries a different agent build (auto-update).
param([Parameter(Mandatory = $true)][string]$Media)

$ErrorActionPreference = 'Continue'
$logDir = Join-Path $env:ProgramData 'LSW'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
$log = Join-Path $logDir 'install-agent.log'
function Log([string]$m) { Add-Content -Path $log -Value ("{0:o} {1}" -f (Get-Date), $m) }
# Exits 1 when a required step fails; the caller reports it to the host.
function Fail([string]$m) { Log "FAILED: $m"; exit 1 }

Log "install from $Media"

# WinFsp: the user-mode file system layer virtio-fs shared folders need.
$winfsp = Join-Path $Media 'lsw\winfsp.msi'
$winfspDll = Join-Path ${env:ProgramFiles(x86)} 'WinFsp\bin\winfsp-x64.dll'
if ((Test-Path $winfsp) -and -not (Test-Path $winfspDll)) {
    $p = Start-Process msiexec.exe -ArgumentList "/i `"$winfsp`" /qn /norestart /l*v `"$logDir\winfsp.log`"" -Wait -PassThru
    Log "winfsp exit $($p.ExitCode)"
    # 3010: success, reboot required.
    if ($p.ExitCode -ne 0 -and $p.ExitCode -ne 3010) { Fail "WinFsp installer exit $($p.ExitCode)" }
}
if (-not (Test-Path $winfspDll)) { Fail "WinFsp is neither installed nor on this medium" }

# virtio-win's own service would claim the first virtio-fs device; the LSW
# agent mounts every share itself.
Stop-Service VirtioFsSvc -ErrorAction SilentlyContinue
Set-Service VirtioFsSvc -StartupType Disabled -ErrorAction SilentlyContinue

$dest = Join-Path $env:ProgramFiles 'LSW'
New-Item -ItemType Directory -Force -Path $dest | Out-Null
Stop-ScheduledTask -TaskName 'LSW Agent' -ErrorAction SilentlyContinue
Get-Process lsw-agent -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 1
try {
    Copy-Item -Force -ErrorAction Stop (Join-Path $Media 'lsw\agent\lsw-agent.exe') $dest
    Copy-Item -Force -ErrorAction Stop (Join-Path $Media 'lsw\version.txt') $dest
} catch { Fail "copying the agent: $_" }

# Start at logon in the user's session, elevated, restarted if it dies.
$user = "$env:USERDOMAIN\$env:USERNAME"
$action = New-ScheduledTaskAction -Execute (Join-Path $dest 'lsw-agent.exe')
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1) -MultipleInstances IgnoreNew
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Highest
try {
    Register-ScheduledTask -TaskName 'LSW Agent' -Action $action -Trigger $trigger -Settings $settings -Principal $principal -Force -ErrorAction Stop | Out-Null
    Start-ScheduledTask -TaskName 'LSW Agent' -ErrorAction Stop
} catch { Fail "registering the agent task: $_" }
Log ("agent {0} installed" -f (Get-Content (Join-Path $dest 'version.txt')))
exit 0
