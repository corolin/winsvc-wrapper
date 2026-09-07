# Runtime smoke test: wrap real interpreters (node/python/java) with rsw in
# foreground mode. No admin required. Verifies bare-command PATH lookup,
# argument escaping (a space-containing arg must arrive intact), output
# capture, and graceful shutdown via console ctrl event.
# Run: powershell -ExecutionPolicy Bypass -File scripts\test-runtimes.ps1

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$rsw = Join-Path $root 'target\debug\rsw.exe'
$testChild = Join-Path $root 'target\debug\test-child.exe'
$fixtures = Join-Path $root 'tests\fixtures'

if (-not (Test-Path $rsw)) { throw "build first: cargo build --bins ($rsw missing)" }
foreach ($tool in 'node', 'python', 'java') {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { throw "$tool not on PATH" }
}

function Step($name) { Write-Host "`n=== $name ===" -ForegroundColor Cyan }
# NOTE: call as `Must (condition) 'message'` — a comma inside the parens
# changes the parse (comma binds tighter than -eq) and swallows $what.
function Must($cond, $what) { if (-not $cond) { throw "FAIL: $what" } else { Write-Host "  ok: $what" -ForegroundColor Green } }

$fixtures = $fixtures.Replace('\', '/')
$failed = $false
foreach ($rt in @(
        @{ exe = 'node';   args = @('hello_service.js', 'hello world', '--flag') }
        @{ exe = 'python'; args = @('-u', 'hello_service.py', 'hello world', '--flag') }
        @{ exe = 'java';   args = @('hello_service.java', 'hello world', '--flag') }
    )) {
    $name = $rt.exe
    Step "$name"
    $dir = Join-Path $env:TEMP ("rsw-rt-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
    New-Item -ItemType Directory -Path (Join-Path $dir 'logs') -Force | Out-Null
    $cfg = Join-Path $dir 'app.toml'
    $argsToml = ($rt.args | ForEach-Object { '"' + ($_ -replace '\\', '\\') + '"' }) -join ', '
    @"
[service]
id = "rsw-rt-$name"

[process]
executable = "$name"          # bare name: resolved through PATH
arguments = [$argsToml]       # 'hello world' must arrive as ONE argument
working_dir = "$fixtures"
stop_signal = "ctrl-c"        # the smoke asserts graceful CLEAN-SHUTDOWN
stop_timeout_secs = 10

[logging]
dir = "$($dir.Replace('\', '/'))/logs"
mode = "append"
"@ | Set-Content $cfg

    try {
        $proc = Start-Process -FilePath $rsw -ArgumentList 'run', $cfg -WindowStyle Hidden -PassThru
        $p = $proc.Id
        # wait for READY (java compiles the source file on first run)
        $deadline = (Get-Date).AddSeconds(40)
        $out = ''
        while ((Get-Date) -lt $deadline) {
            Start-Sleep -Milliseconds 200
            $out = (Get-Content (Join-Path $dir 'logs\app.out.log') -Raw -ErrorAction SilentlyContinue) -join "`n"
            if ($out -match 'READY') { break }
        }
        Must ($out -match 'READY') "${name} started (READY)"
        Must ($out -match 'ARG0=hello world') "${name} received the space-containing arg as one string"
        Must ($out -match 'ARG1=--flag') "${name} received the trailing flag"
        # wait for steady-state ticking before asserting on it
        $deadline = (Get-Date).AddSeconds(10)
        while ((Get-Date) -lt $deadline) {
            $out = (Get-Content (Join-Path $dir 'logs\app.out.log') -Raw -ErrorAction SilentlyContinue) -join "`n"
            if ($out -match 'TICK 2') { break }
            Start-Sleep -Milliseconds 200
        }
        Must ($out -match 'TICK 2') "${name} long-running output captured"

        & $testChild ctrl-c $p | Out-Null
        $exited = $proc.WaitForExit(15000)
        Must $exited "${name}: rsw exited after ctrl-c"
        Must ($proc.ExitCode -eq 0) "${name}: rsw exit code 0"

        $out = (Get-Content (Join-Path $dir 'logs\app.out.log') -Raw -ErrorAction SilentlyContinue) -join "`n"
        Must ($out -match 'CLEAN-SHUTDOWN') "${name} shut down gracefully (child ran its own cleanup)"
        $wrapper = (Get-Content (Join-Path $dir 'logs\app.wrapper.log') -Raw -ErrorAction SilentlyContinue) -join "`n"
        Must ($wrapper -match 'stop requested') "${name}: deliberate stop recorded"
        Must ($wrapper -match 'child exited after signal|already exited') "${name}: signal reached the child"
    } catch {
        $failed = $true
        Write-Host "FAIL ($name): $_" -ForegroundColor Red
        Get-Content (Join-Path $dir 'logs\app.wrapper.log') -ErrorAction SilentlyContinue | Write-Host
        Get-Content (Join-Path $dir 'logs\app.out.log') -ErrorAction SilentlyContinue | Write-Host
    } finally {
        if (Get-Process -Id $p -ErrorAction SilentlyContinue) { Stop-Process -Id $p -Force -ErrorAction SilentlyContinue }
        Remove-Item $dir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if ($failed) { throw "runtime smoke test failed" }
Write-Host "`nALL RUNTIME CHECKS PASSED" -ForegroundColor Green
