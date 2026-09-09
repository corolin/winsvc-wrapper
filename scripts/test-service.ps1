# rsw SCM roundtrip regression. Run from an elevated PowerShell:
#   powershell -ExecutionPolicy Bypass -File scripts\test-service.ps1
# Requires: cargo build (debug is fine).

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$rsw = Join-Path $root 'target\debug\rsw.exe'
$testChild = Join-Path $root 'target\debug\test-child.exe'
$dir = Join-Path $env:TEMP ("rsw-scm-test-" + [guid]::NewGuid().ToString('N').Substring(0, 8))

if (-not (Test-Path $rsw)) { throw "build first: cargo build --bins ($rsw missing)" }
$isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole('Administrators')
if (-not $isAdmin) { throw "this script must run in an elevated prompt" }

New-Item -ItemType Directory -Path (Join-Path $dir 'logs') -Force | Out-Null
$cfg = Join-Path $dir 'scm.toml'
@"
[service]
id = "rsw-scm-test"
name = "rsw SCM Test"
description = "installed by test-service.ps1"
start_type = "manual"

[process]
executable = "$($testChild.Replace('\', '/'))"
arguments = ["graceful", "200"]
stop_signal = "ctrl-c"    # the default is "kill"; the GRACEFUL-DONE check below needs a real signal
stop_timeout_secs = 10

[[on_failure]]
action = "restart"
delay = "2 sec"

[logging]
dir = "logs"
mode = "append"
"@ | Set-Content $cfg

function Step($name) { Write-Host "`n=== $name ===" -ForegroundColor Cyan }
# NOTE: call as `Must (condition) 'message'` — a comma inside the parens
# would change the parse (comma binds tighter than -eq) and swallow $what.
function Must($cond, $what) { if (-not $cond) { throw "FAIL: $what" } else { Write-Host "  ok: $what" -ForegroundColor Green } }

try {
    Step 'install'
    & $rsw install $cfg | Write-Host
    Must (Get-Service 'rsw-scm-test' -ErrorAction SilentlyContinue) 'service visible in Get-Service'

    Step 'status (stopped)'
    & $rsw status $cfg | Write-Host
    Must ($LASTEXITCODE -eq 0) 'status exit 0 while stopped'

    Step 'failure actions persisted'
    # Ground truth: the SCM persists failure actions as a binary blob in the
    # registry — locale-independent, no admin needed to read. Layout (verified
    # against wuauserv/Dhcp/Netman):
    #   0: reset_period (s)   4: reboot_msg offset   8: command offset
    #   12: entry count       16: actions byte offset (20 when no strings)
    #   20+: 8-byte entries (u32 type, u32 delay_ms); type 0=none 1=restart 2=reboot
    $fa = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Services\rsw-scm-test' -Name FailureActions -ErrorAction Stop).FailureActions
    $resetPeriod  = [BitConverter]::ToUInt32($fa, 0)
    $actionsStart = [BitConverter]::ToUInt32($fa, 16)
    if ($actionsStart -lt 20 -or $actionsStart % 4 -ne 0) { $actionsStart = 20 }
    $found = $false
    for ($off = $actionsStart; $off + 8 -le $fa.Length; $off += 8) {
        $type  = [BitConverter]::ToUInt32($fa, $off)
        $delay = [BitConverter]::ToUInt32($fa, $off + 4)
        Write-Host ("  registry entry: type={0} delay={1}ms" -f $type, $delay)
        if ($type -eq 1 -and $delay -eq 2000) { $found = $true }
    }
    Must ($resetPeriod -eq 86400) 'failure reset period persisted (86400s)'
    Must $found 'restart action (delay 2000ms) persisted in registry'

    Step 'start'
    & $rsw start $cfg | Write-Host
    Must ($LASTEXITCODE -eq 0) 'start exit 0'
    Start-Sleep 3
    & $rsw status $cfg | Write-Host
    Must ($LASTEXITCODE -eq 0) 'status exit 0 while running'
    Must ((Get-Service 'rsw-scm-test').Status -eq 'Running') 'service Running'
    Must (Test-Path (Join-Path $dir 'logs\scm.out.log')) 'child stdout captured under the service account'

    Step 'stop (graceful)'
    & $rsw stop $cfg | Write-Host
    Must ($LASTEXITCODE -eq 0) 'stop exit 0'
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Service 'rsw-scm-test').Status -ne 'Stopped' -and (Get-Date) -lt $deadline) { Start-Sleep 1 }
    Must ((Get-Service 'rsw-scm-test').Status -eq 'Stopped') 'service Stopped'
    $out = Get-Content (Join-Path $dir 'logs\scm.out.log') -Raw
    Must ($out -match 'GRACEFUL-DONE') 'child shut down gracefully via SCM stop'
    $wl = Get-Content (Join-Path $dir 'logs\scm.wrapper.log') -Raw
    Must ($wl -match 'sent CtrlC to child') 'rsw delivered the ctrl event itself (no console broadcast from outside)'

    Step 'refresh'
    (Get-Content $cfg -Raw).Replace('name = "rsw SCM Test"', 'name = "rsw SCM Test (refreshed)"') | Set-Content $cfg
    & $rsw refresh $cfg | Write-Host
    Must ((Get-Service 'rsw-scm-test').DisplayName -match 'refreshed') 'display name updated by refresh'

    Step 'uninstall'
    & $rsw uninstall $cfg | Write-Host
    Must (-not (Get-Service 'rsw-scm-test' -ErrorAction SilentlyContinue)) 'service removed'

    Write-Host "`nALL SCM CHECKS PASSED" -ForegroundColor Green
} finally {
    if (Get-Service 'rsw-scm-test' -ErrorAction SilentlyContinue) {
        Stop-Service 'rsw-scm-test' -Force -ErrorAction SilentlyContinue
        sc.exe delete rsw-scm-test | Out-Null
    }
    Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
}
