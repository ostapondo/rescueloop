$ErrorActionPreference = "Stop"
$Root = Join-Path ([System.IO.Path]::GetTempPath()) ("rescueloop-e2e-" + [guid]::NewGuid())
$PreviousRustLog = $env:RUST_LOG
$env:RUST_LOG = "info"
$ServiceInstalled = $false
try {
    New-Item -ItemType Directory -Force -Path $Root | Out-Null
    $State = Join-Path $Root ".rescueloop"
    $Incidents = Join-Path $State "incidents"
    & cargo run --quiet -p rescueloop -- --incident-dir $Incidents run cmd.exe /c exit 42
    if ($LASTEXITCODE -ne 0) { throw "supervised run command failed" }
    $Files = @(Get-ChildItem $Incidents -Filter *.json)
    if ($Files.Count -ne 1) { throw "expected exactly one incident, found $($Files.Count)" }
    $Incident = Get-Content $Files[0].FullName -Raw | ConvertFrom-Json
    if ($Incident.kind -ne "abnormal_exit") { throw "unexpected incident kind: $($Incident.kind)" }
    & cargo run --quiet -p rescueloop -- --incident-dir $Incidents sources list
    if ($LASTEXITCODE -ne 0) { throw "sources command failed" }
    & cargo run --quiet -p rescueloop -- --incident-dir $Incidents replay (Join-Path $Root "missing.json") 2>$null
    if ($LASTEXITCODE -eq 0) { throw "expected replay failure" }
    $LogFiles = @(Get-ChildItem (Join-Path $State "logs") -Filter "rescueloop-*.jsonl")
    if ($LogFiles.Count -lt 1) { throw "expected operational log file" }
    $LogRecords = @(Get-Content $LogFiles[-1].FullName | ForEach-Object { $_ | ConvertFrom-Json })
    if (-not ($LogRecords | Where-Object { $_.fields.event -eq "runtime.failed" })) {
        throw "runtime.failed log event not found"
    }
    if ($LogRecords | Where-Object { -not $_.schema_version -or -not $_.run_id -or -not $_.correlation_id }) {
        throw "log context fields are incomplete"
    }
    $env:RESCUELOOP_TEST_PANIC = "1"
    & cargo run --quiet -p rescueloop -- --incident-dir $Incidents sources list 2>$null
    Remove-Item Env:RESCUELOOP_TEST_PANIC
    if ($LASTEXITCODE -eq 0) { throw "expected debug panic" }
    $Binary = Join-Path (Get-Location) "target/debug/rescueloop.exe"
    $Parallel = 1..8 | ForEach-Object {
        Start-Process -FilePath $Binary -ArgumentList @("--incident-dir", $Incidents, "sources", "list") -PassThru -WindowStyle Hidden
    }
    $Parallel | Wait-Process
    if ($Parallel | Where-Object { $_.ExitCode -ne 0 }) { throw "parallel logging process failed" }
    $LogRecords = @(& cargo run --quiet -p rescueloop -- --incident-dir $Incidents logs --lines 1000 --output json | ForEach-Object { $_ | ConvertFrom-Json })
    if (-not ($LogRecords | Where-Object { $_.fields.event -eq "runtime.panic" })) {
        throw "runtime.panic log event not found"
    }
    if (@($LogRecords.run_id | Sort-Object -Unique).Count -lt 3) {
        throw "expected distinct run IDs across process restarts"
    }
    $env:RESCUELOOP_TEST_ABORT_AFTER_OCCURRENCE = "1"
    & cargo run --quiet -p rescueloop -- --incident-dir $Incidents run cmd.exe /c exit 43 2>$null
    Remove-Item Env:RESCUELOOP_TEST_ABORT_AFTER_OCCURRENCE
    if ($LASTEXITCODE -eq 0) { throw "expected observation failpoint abort" }
    $Pending = @(Get-ChildItem (Join-Path $State "observation-journal") -Filter *.json)
    if ($Pending.Count -ne 1) { throw "expected one pending observation transaction" }
    & cargo run --quiet -p rescueloop -- --incident-dir $Incidents run cmd.exe /c exit 43
    if ($LASTEXITCODE -ne 0) { throw "observation recovery command failed" }
    $Pending = @(Get-ChildItem (Join-Path $State "observation-journal") -Filter *.json)
    if ($Pending.Count -ne 0) { throw "observation journal was not drained" }
    $Recovered = @(Get-ChildItem $Incidents -Filter *.json | ForEach-Object {
        Get-Content $_.FullName -Raw | ConvertFrom-Json
    } | Where-Object { $_.normalized_failure.code -eq "exit:43" })
    if ($Recovered.Count -ne 1 -or $Recovered[0].occurrence_count -ne 2) {
        throw "observation recovery was not idempotent"
    }
    & cargo run --quiet -p rescueloop -- --incident-dir $Incidents logs --verify --lines 0
    if ($LASTEXITCODE -ne 0) { throw "log integrity verification failed" }
    & cargo run --quiet -p rescueloop -- service status
    if ($LASTEXITCODE -ne 0) { throw "service status failed" }

    function Assert-Health([string]$ExpectedWatcherState) {
        $Deadline = [DateTime]::UtcNow.AddSeconds(15)
        do {
            $HealthText = & $Binary --incident-dir $Incidents doctor --json
            if ($LASTEXITCODE -eq 0) {
                $Health = $HealthText | ConvertFrom-Json
                $Watcher = $Health.checks | Where-Object { $_.name -eq "watcher" }
                $Protected = [uint64]$Health.persisted + [uint64]$Health.grouped + [uint64]$Health.deduplicated + [uint64]$Health.journal_pending
                if ($Watcher.state -eq $ExpectedWatcherState -and $Health.queue_depth -le $Health.queue_capacity -and $Health.received -le $Protected) {
                    return $Health
                }
            }
            Start-Sleep -Milliseconds 250
        } while ([DateTime]::UtcNow -lt $Deadline)
        throw "watcher did not reach $ExpectedWatcherState health: $HealthText"
    }

    & $Binary --incident-dir $Incidents service install
    if ($LASTEXITCODE -ne 0) { throw "scheduled task installation failed" }
    $ServiceInstalled = $true
    $InitialHealth = Assert-Health "healthy"
    $InitialPid = (Get-Content (Join-Path $State "watch-health-v1.json") -Raw | ConvertFrom-Json).pid

    $Task = Get-ScheduledTask -TaskName "RescueLoop"
    if (-not ($Task.Triggers | Where-Object { $_.CimClass.CimClassName -eq "MSFT_TaskLogonTrigger" })) {
        throw "scheduled task has no logon trigger"
    }

    & schtasks /End /TN RescueLoop | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "schtasks /End failed" }
    $null = Assert-Health "degraded"
    & $Binary restart
    if ($LASTEXITCODE -ne 0) { throw "watcher restart failed" }
    $null = Assert-Health "healthy"
    $RestartedPid = (Get-Content (Join-Path $State "watch-health-v1.json") -Raw | ConvertFrom-Json).pid
    if ($RestartedPid -eq $InitialPid) { throw "restart did not replace the watcher process" }

    # Exercise the /End -> /Run race with concurrent idempotent starts.
    & schtasks /End /TN RescueLoop | Out-Null
    $Starts = 1..2 | ForEach-Object {
        Start-Process -FilePath $Binary -ArgumentList @("--incident-dir", $Incidents, "start") -PassThru -WindowStyle Hidden
    }
    $Starts | Wait-Process
    if ($Starts | Where-Object { $_.ExitCode -ne 0 }) { throw "concurrent watcher start failed" }
    $null = Assert-Health "healthy"

    # Force a non-English UI culture; numeric ScheduledTaskState remains stable.
    $LocalizedState = & powershell -NoProfile -NonInteractive -Command "[cultureinfo]::CurrentUICulture=[cultureinfo]'pl-PL'; [int](Get-ScheduledTask -TaskName 'RescueLoop').State"
    if ([int]$LocalizedState -ne 4) { throw "localized scheduled task state was not running" }

    # Corrupt the disposable index, then prove doctor rebuilds it from incident JSON.
    Set-Content -LiteralPath (Join-Path $State "index-v1.db") -Value "not sqlite" -NoNewline
    $Rebuilt = & $Binary --incident-dir $Incidents doctor --json | ConvertFrom-Json
    if (($Rebuilt.checks | Where-Object { $_.name -eq "SQLite projection" }).state -ne "healthy") {
        throw "corrupted index was not rebuilt"
    }
    if (@(Get-ChildItem $Incidents -Filter *.json).Count -ne 2) {
        throw "index recovery changed the durable incident count"
    }
    Write-Host "Windows native E2E passed."
}
finally {
    if ($ServiceInstalled) {
        & $Binary service uninstall *> $null
    }
    if ($null -eq $PreviousRustLog) {
        Remove-Item Env:RUST_LOG -ErrorAction SilentlyContinue
    } else {
        $env:RUST_LOG = $PreviousRustLog
    }
    if (Test-Path $Root) { Remove-Item -Recurse -Force $Root }
}
