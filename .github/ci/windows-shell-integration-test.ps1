# Disposable Windows CI runner only. Verify that the shell integration writes
# the cmd shims and the user PATH entry, that the shims dispatch into the CLI,
# and that removal restores the previous user PATH.
#
# The job log is not readable from the public API, so each phase publishes a
# notice annotation and a failure publishes one error annotation.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

function Invoke-Cli([string[]]$arguments) {
    $output = & $script:cli @arguments 2>&1
    $code = $LASTEXITCODE
    if ($code -ne 0) {
        throw ('localweb {0} exited {1}: {2}' -f ($arguments -join ' '), $code, (($output | Out-String).Trim()))
    }
    return (($output | Out-String).Trim())
}

& cargo build -p localweb-cli
if ($LASTEXITCODE -ne 0) {
    Write-Host '::error title=Windows shell integration (build)::Could not build the CLI.'
    throw 'Could not build the CLI.'
}

$script:cli = (Resolve-Path target/debug/localweb.exe).Path
$root = Join-Path $env:RUNNER_TEMP ('localweb-shell-' + [guid]::NewGuid())
$previousData = $env:LOCALAPPDATA
$previousProcessPath = $env:PATH
$previousUserPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
New-Item -ItemType Directory -Path $root | Out-Null
try {
    $env:LOCALAPPDATA = Join-Path $root 'data'
    $shimDir = Join-Path $env:LOCALAPPDATA 'LocalWeb\data\bin'
    $tools = @('php', 'composer', 'node', 'npm', 'npx')

    $status = Invoke-Cli @('integration', 'status')
    if ($status -notmatch 'Shims: missing') {
        throw "unexpected status before install: $status"
    }

    $null = Invoke-Cli @('integration', 'install')
    foreach ($tool in $tools) {
        $shim = Join-Path $shimDir "$tool.cmd"
        if (-not (Test-Path $shim)) {
            throw "install did not create $shim"
        }
        $body = Get-Content -Raw $shim
        if ($body -notmatch 'LocalWeb shell integration') {
            throw "unmarked shim $shim"
        }
        if ($body -notmatch " shim $tool -- ") {
            throw "shim $shim does not dispatch $tool"
        }
    }
    $userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
    $entries = @(([string]$userPath) -split ';' | Where-Object { $_ -eq $shimDir })
    if ($entries.Count -ne 1) {
        throw "the user PATH does not contain $shimDir exactly once: $userPath"
    }
    $null = Invoke-Cli @('integration', 'install')
    $userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
    $entries = @(([string]$userPath) -split ';' | Where-Object { $_ -eq $shimDir })
    if ($entries.Count -ne 1) {
        throw 'a second install duplicated the user PATH entry'
    }
    $status = Invoke-Cli @('integration', 'status')
    if ($status -notmatch 'Shims: installed') {
        throw "unexpected status after install: $status"
    }
    if ($status -notmatch 'User PATH: configured') {
        throw "status does not report the user PATH: $status"
    }
    Write-Host '::notice title=Windows shell integration::install writes shims and one PATH entry'

    $systemDir = Join-Path $root 'system'
    New-Item -ItemType Directory -Path $systemDir | Out-Null
    Set-Content -Path (Join-Path $systemDir 'php.cmd') -Value "@echo off`r`necho system php %*`r`nexit /b 0`r`n"
    $env:PATH = "$systemDir;$env:PATH"
    $output = & (Join-Path $shimDir 'php.cmd') -v 2>&1
    $code = $LASTEXITCODE
    if ($code -ne 0) {
        throw "the php shim exited $code`: $output"
    }
    if (("$output" -join ' ') -notmatch 'system php -v') {
        throw "the php shim did not fall back to the system php: $output"
    }
    Write-Host '::notice title=Windows shell integration::shims dispatch and fall back'

    $null = Invoke-Cli @('integration', 'remove')
    foreach ($tool in $tools) {
        if (Test-Path (Join-Path $shimDir "$tool.cmd")) {
            throw "remove left the $tool shim"
        }
    }
    if (Test-Path $shimDir) {
        throw 'remove left the shim directory'
    }
    $userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
    $entries = @(([string]$userPath) -split ';' | Where-Object { $_ -eq $shimDir })
    if ($entries.Count -ne 0) {
        throw 'remove left the user PATH entry'
    }
    $status = Invoke-Cli @('integration', 'status')
    if ($status -notmatch 'Shims: missing') {
        throw "unexpected status after remove: $status"
    }
    Write-Host '::notice title=Windows shell integration::install and remove are symmetric'
} catch {
    $flat = ($_.Exception.Message -replace "`r", ' ' -replace "`n", ' ~ ')
    if ($flat.Length -gt 900) {
        $flat = '...' + $flat.Substring($flat.Length - 900)
    }
    Write-Host ('::error title=Windows shell integration::{0}' -f $flat.Replace('%', '%25'))
    throw
} finally {
    if ($null -eq $previousUserPath) {
        & reg delete 'HKCU\Environment' /v PATH /f 2>&1 | Out-Null
    } else {
        & reg add 'HKCU\Environment' /v PATH /t REG_EXPAND_SZ /d $previousUserPath /f 2>&1 | Out-Null
    }
    $env:LOCALAPPDATA = $previousData
    $env:PATH = $previousProcessPath
    Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
}
