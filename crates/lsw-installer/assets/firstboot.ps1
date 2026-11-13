# LSW first-boot script. Runs once, at the first logon after unattended setup.
# Progress is reported on COM1 as "LSW_STAGE <name>" lines; the host waits for
# "LSW_INSTALL_DONE". Every step logs to C:\ProgramData\LSW\firstboot.log.
param([Parameter(Mandatory = $true)][string]$Media)

$ErrorActionPreference = 'Continue'
$logDir = Join-Path $env:ProgramData 'LSW'
New-Item -ItemType Directory -Force -Path $logDir | Out-Null
$log = Join-Path $logDir 'firstboot.log'

function Send-Serial([string]$line) {
    try {
        $port = New-Object System.IO.Ports.SerialPort 'COM1', 115200, 'None', 8, 'One'
        $port.Open()
        $port.WriteLine($line)
        $port.Close()
    } catch {
        Add-Content -Path $log -Value "serial: $_"
    }
}

function Stage([string]$name) {
    Add-Content -Path $log -Value ("{0:o} {1}" -f (Get-Date), $name)
    Send-Serial "LSW_STAGE $name"
}

Stage 'start'

$config = @{ test_signing = $false }
$configPath = Join-Path $Media 'lsw\config.json'
if (Test-Path $configPath) {
    $config = Get-Content -Raw $configPath | ConvertFrom-Json
}

# 1. virtio-win drivers and services (storage, network, balloon, serial, fs, input, gpu).
$msi = $null
foreach ($d in [char[]]'DEFGHIJK') {
    $candidate = "${d}:\virtio-win-gt-x64.msi"
    if (Test-Path $candidate) { $msi = $candidate; break }
}
if ($msi) {
    Stage 'virtio-drivers'
    $p = Start-Process msiexec.exe -ArgumentList "/i `"$msi`" /qn /norestart /l*v `"$logDir\virtio-win.log`"" -Wait -PassThru
    Stage "virtio-drivers-exit-$($p.ExitCode)"
} else {
    Stage 'virtio-drivers-missing'
}

# 2. Keep autologon permanently (Windows decrements AutoLogonCount).
Stage 'autologon'
$winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
Set-ItemProperty -Path $winlogon -Name AutoAdminLogon -Value '1'
Remove-ItemProperty -Path $winlogon -Name AutoLogonCount -ErrorAction SilentlyContinue

# 3. Windows Update must not replace LSW's GPU and virtio drivers.
Stage 'driver-updates'
$wu = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate'
New-Item -Path $wu -Force | Out-Null
Set-ItemProperty -Path $wu -Name ExcludeWUDriversInQualityUpdate -Type DWord -Value 1

# 4. Power: the host pauses and saves the VM; Windows must not sleep itself.
Stage 'power'
powercfg.exe /hibernate off
powercfg.exe /change standby-timeout-ac 0
powercfg.exe /change monitor-timeout-ac 0

# 5. Test signing for the alpha's paravirtualized 3D driver (needs Secure Boot off).
if ($config.test_signing) {
    Stage 'test-signing'
    bcdedit.exe /set testsigning on | Out-File -Append $log
}

# 6. LSW guest agent and WinFsp (LSW Tools).
$installer = Join-Path $Media 'lsw\install-agent.ps1'
if (Test-Path $installer) {
    Stage 'agent'
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $installer -Media $Media
    Stage "agent-exit-$LASTEXITCODE"
}

# 7. The answer file contained the account password; remove every copy.
Stage 'cleanup'
Remove-Item -Force -ErrorAction SilentlyContinue 'C:\Windows\Panther\unattend.xml', 'C:\Windows\Panther\Unattend\unattend.xml'

Stage 'done'
Send-Serial 'LSW_INSTALL_DONE'
