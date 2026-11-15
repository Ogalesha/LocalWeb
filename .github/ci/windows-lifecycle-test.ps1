# Disposable Windows CI runner only. Verify manual start with login autostart
# off, restart ownership, and a graceful shutdown of the daemon this build owns.
#
# The job log is not readable from the public API, so every phase publishes a
# notice annotation and a failure publishes one error annotation with the
# command output, the task state and the daemon log tail.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$script:phase = 'build'
$script:trace = [System.Collections.Generic.List[string]]::new()
$script:lastOutput = ''
$script:lastStdout = ''
$script:lastExit = 0

function Add-Trace([string]$message) {
    $script:trace.Add("$($script:phase): $message")
    Write-Host "[lifecycle] $($script:phase): $message"
}

function Set-Phase([string]$name) {
    $script:phase = $name
    Add-Trace 'begin'
    Write-Host "::notice title=Windows lifecycle::$name"
}

function Format-Annotation([string]$text, [int]$limit = 950) {
    $flat = $text -replace "`r", ' ' -replace "`n", ' ~ '
    if ($flat.Length -gt $limit) {
        $flat = '...' + $flat.Substring($flat.Length - $limit)
    }
    return $flat.Replace('%', '%25')
}

# Runs the CLI with redirected output files (never a PowerShell pipeline, so a
# daemon that inherits a handle cannot hold the step open) and a hard timeout.
function Invoke-Cli([string[]]$arguments, [string]$purpose, [int]$timeoutSeconds = 120) {
    $label = 'localweb ' + ($arguments -join ' ')
    $stdoutPath = Join-Path $env:TEMP ([guid]::NewGuid().ToString('N') + '.out')
    $stderrPath = Join-Path $env:TEMP ([guid]::NewGuid().ToString('N') + '.err')
    try {
        $process = Start-Process -FilePath $cli -ArgumentList $arguments -NoNewWindow -PassThru `
            -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath
        $deadline = (Get-Date).AddSeconds($timeoutSeconds)
        while (-not $process.HasExited -and (Get-Date) -lt $deadline) {
            Start-Sleep -Milliseconds 100
        }
        if (-not $process.HasExited) {
            Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
            $process.WaitForExit(10000) | Out-Null
            $script:lastExit = -1
            $script:lastStdout = ''
            $script:lastOutput = "no exit within $timeoutSeconds seconds"
        } else {
            $script:lastExit = $process.ExitCode
            $script:lastStdout = ''
            $lines = @()
            if (Test-Path $stdoutPath) {
                $stdoutLines = @(Get-Content $stdoutPath)
                $script:lastStdout = $stdoutLines -join "`n"
                $lines += $stdoutLines
            }
            if (Test-Path $stderrPath) {
                $lines += @(Get-Content $stderrPath)
            }
            $script:lastOutput = ($lines | Select-Object -Last 12) -join ' ~ '
        }
    } finally {
        Remove-Item $stdoutPath, $stderrPath -Force -ErrorAction SilentlyContinue
    }
    Add-Trace ("{0} -> {1}" -f $label, $script:lastExit)
    if ($script:lastExit -ne 0) {
        throw ("{0}: {1} exited {2}: {3}" -f $purpose, $label, $script:lastExit, $script:lastOutput)
    }
}

function Invoke-CliQuiet([string[]]$arguments, [int]$timeoutSeconds = 60) {
    try {
        Invoke-Cli -arguments $arguments -purpose 'cleanup' -timeoutSeconds $timeoutSeconds
    } catch {
        Add-Trace "cleanup ignored: $($_.Exception.Message)"
    }
}

function Get-DaemonTask {
    return Get-ScheduledTask -TaskName 'LocalWeb Daemon' -ErrorAction SilentlyContinue
}

function Test-TaskEnabled {
    $task = Get-DaemonTask
    return ($null -ne $task) -and [bool]$task.Settings.Enabled
}

function Wait-TaskEnabled([bool]$enabled) {
    $deadline = (Get-Date).AddSeconds(20)
    do {
        if ((Test-TaskEnabled) -eq $enabled) {
            return $true
        }
        Start-Sleep -Milliseconds 200
    } while ((Get-Date) -lt $deadline)
    return $false
}

function Wait-DaemonReady {
    $deadline = (Get-Date).AddSeconds(20)
    $last = 'no status reply'
    while ((Get-Date) -lt $deadline) {
        try {
            Invoke-Cli -arguments @('status', '--json') -purpose 'reading daemon status' -timeoutSeconds 60
            $status = $script:lastStdout | ConvertFrom-Json
            if ($null -ne $status -and $status.data.ready) {
                return $status
            }
            $last = 'the daemon is still restoring projects'
        } catch {
            $last = $_.Exception.Message
        }
        Start-Sleep -Milliseconds 250
    }
    throw "The manually started daemon was not ready: $last"
}

function Get-DaemonLog {
    $log = Join-Path $env:LOCALAPPDATA 'LocalWeb\state\logs\daemon.log'
    if (-not (Test-Path $log)) {
        return 'daemon.log missing'
    }
    return ((Get-Content $log -Tail 15) -join ' ~ ')
}

& cargo build -p localweb-cli -p localweb-daemon
if ($LASTEXITCODE -ne 0) {
    Write-Host '::error title=Windows lifecycle (build)::Could not build lifecycle fixtures.'
    throw 'Could not build lifecycle fixtures.'
}
$cli = (Resolve-Path target/debug/localweb.exe).Path
$root = Join-Path $env:RUNNER_TEMP ('localweb-lifecycle-' + [guid]::NewGuid())
$previousData = $env:LOCALAPPDATA
$previousRuntime = $env:LOCALWEB_RUNTIME_DIR
$unrelated = $null
if (Get-DaemonTask) {
    Write-Host '::error title=Windows lifecycle (fixture)::Runner already has a LocalWeb task.'
    throw 'Lifecycle fixture requires a fresh runner without a LocalWeb task.'
}
New-Item -ItemType Directory -Path $root | Out-Null
try {
    $env:LOCALAPPDATA = Join-Path $root 'data'
    $env:LOCALWEB_RUNTIME_DIR = Join-Path $root 'runtime'

    Set-Phase 'create disabled login task'
    Invoke-Cli -arguments @('autostart', 'disable') -purpose 'creating the disabled login task' -timeoutSeconds 90
    if (-not (Wait-TaskEnabled $false)) {
        throw 'Autostart was not disabled.'
    }

    Set-Phase 'manual start with autostart off'
    $dummy = Join-Path $root 'localwebd.exe'
    Copy-Item (Join-Path ([Environment]::SystemDirectory) 'ping.exe') $dummy
    $unrelated = Start-Process $dummy -ArgumentList @('-t', '127.0.0.1') -WindowStyle Hidden -PassThru
    Invoke-Cli -arguments @('start') -purpose 'starting the daemon manually' -timeoutSeconds 150
    $null = Wait-DaemonReady
    $unrelated.Refresh()
    if ($unrelated.HasExited) {
        throw 'Manual start killed an unrelated same-name process.'
    }
    if (Test-TaskEnabled) {
        throw 'Manual start changed the login preference.'
    }

    Set-Phase 'restart with autostart off'
    Invoke-Cli -arguments @('start') -purpose 'restarting the daemon manually' -timeoutSeconds 150
    $null = Wait-DaemonReady
    $unrelated.Refresh()
    if ($unrelated.HasExited) {
        throw 'Restart killed an unrelated same-name process.'
    }
    if (Test-TaskEnabled) {
        throw 'Restart changed the login preference.'
    }

    Set-Phase 'restart with autostart on'
    Invoke-Cli -arguments @('autostart', 'enable') -purpose 'enabling login autostart' -timeoutSeconds 90
    if (-not (Wait-TaskEnabled $true)) {
        throw 'Could not enable login autostart.'
    }
    Invoke-Cli -arguments @('start') -purpose 'restarting with autostart enabled' -timeoutSeconds 150
    if (-not (Wait-TaskEnabled $true)) {
        throw 'Restart disabled login autostart.'
    }

    Set-Phase 'shut down the owned daemon'
    Invoke-Cli -arguments @('shutdown') -purpose 'shutting down gracefully' -timeoutSeconds 120
    $unrelated.Refresh()
    if ($unrelated.HasExited) {
        throw 'Shutdown killed an unrelated process.'
    }
} catch {
    $details = @(
        "phase=$($script:phase)",
        "exception=$($_.Exception.Message)"
    )
    if ($script:lastOutput) {
        $details += "last-output=$($script:lastOutput)"
    }
    if (Get-DaemonTask) {
        $details += "task-state=$((Get-DaemonTask).State)/enabled=$(Test-TaskEnabled)"
    } else {
        $details += 'task-state=missing'
    }
    $details += "log=$(Get-DaemonLog)"
    $script:trace.Add("FAILED: $($_.Exception.Message)")
    Write-Host ('::error title=Windows lifecycle ({0})::{1}' -f $script:phase, (Format-Annotation ($details -join ' ~ ')))
    throw
} finally {
    Invoke-CliQuiet -arguments @('shutdown') -timeoutSeconds 60
    if ($unrelated -and -not $unrelated.HasExited) {
        Stop-Process -Id $unrelated.Id -Force -ErrorAction SilentlyContinue
    }
    Unregister-ScheduledTask -TaskName 'LocalWeb Daemon' -Confirm:$false -ErrorAction SilentlyContinue
    $env:LOCALAPPDATA = $previousData
    $env:LOCALWEB_RUNTIME_DIR = $previousRuntime
    Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
    Write-Host ('::notice title=Windows lifecycle trace::{0}' -f (Format-Annotation ($script:trace -join ' ~ ')))
}
