param([Parameter(Mandatory = $true)][string]$Spec)
$ErrorActionPreference = 'Stop'
$request = $null
$runInvoked = $false
# Startup is one operation: from this process's own start, with the helper
# compile, through the resumed workload. A watcher in its own runspace names it
# once while it runs, unless the parent already names the operation this helper
# serves; the native helper reports when the workload has resumed. The watcher
# and the settlement share one claim, so a startup that settles after ten
# seconds, between two of the watcher's looks, is still named once. Elapsed
# time counts from the launcher's spawn of this process on the Windows
# performance counter, which the launcher's high-resolution time and this
# Stopwatch both read, so no clock change ever moves it. Without that mark,
# it counts from this script's start.
$monotonicNs = { [decimal][Diagnostics.Stopwatch]::GetTimestamp() * 1000000000 / [Diagnostics.Stopwatch]::Frequency }
$startedNs = [decimal]0
if (-not [decimal]::TryParse([string]$env:ENGRAM_LAUNCHER_JOB_STARTED_NS, [ref]$startedNs)) { $startedNs = & $monotonicNs }
$startup = [hashtable]::Synchronized(@{ Done = $false; Warned = $false; Watching = $false;
    StartedNs = $startedNs; ProcessId = $PID })
# Returns the elapsed milliseconds when this call claims the one warning.
$claimStartupWarning = {
    param($state, [bool]$settle)
    [Threading.Monitor]::Enter($state.SyncRoot)
    try {
        if ($state.Done) { return $null }
        if ($settle) { $state.Done = $true }
        if (-not $state.Watching -or $state.Warned) { return $null }
        $elapsed = [long](([decimal][Diagnostics.Stopwatch]::GetTimestamp() * 1000000000 / [Diagnostics.Stopwatch]::Frequency - $state.StartedNs) / 1000000)
        if ($elapsed -lt 10000) { return $null }
        $state.Warned = $true
        return $elapsed
    } finally { [Threading.Monitor]::Exit($state.SyncRoot) }
}
# Warning lines go out through a stderr stream of their own, never through the
# shared Console.Error writer, so a slow warning write never holds the lock the
# helper's own error output needs.
$warnStartup = {
    param($state, $elapsed)
    try {
        $bytes = [Text.Encoding]::UTF8.GetBytes("warning: job helper startup is still running after 10 seconds; attempt=$($state.ProcessId)/0; elapsed_ms=$elapsed" + [Environment]::NewLine)
        $stream = [Console]::OpenStandardError()
        try { $stream.Write($bytes, 0, $bytes.Length); $stream.Flush() } finally { $stream.Dispose() }
    } catch { }
}
# The settlement and the receipt step run on the thread doing the work, so
# their lines go to the supervisor's own warning writer. Before that writer is
# loaded, when the helper is already failing, a line is written from its own
# runspace instead; when none can be created, it is not written. Either way
# the thread doing the work never waits for stderr.
$asideWrites = @()
function Write-WarningAside([string]$Line) {
    try {
        $writer = [PowerShell]::Create()
        [void]$writer.AddScript({
            param($text)
            try {
                $bytes = [Text.Encoding]::UTF8.GetBytes($text + [Environment]::NewLine)
                $stream = [Console]::OpenStandardError()
                try { $stream.Write($bytes, 0, $bytes.Length); $stream.Flush() } finally { $stream.Dispose() }
            } catch { }
        }).AddArgument($Line)
        $script:asideWrites += @(@{ Writer = $writer; Pending = $writer.BeginInvoke() })
    } catch { }
}
$reportWarning = {
    param([string]$line)
    try { [LauncherProcessJob]::Report($line); return } catch { }
    Write-WarningAside $line
}
$settleStartup = {
    param($state)
    $elapsed = & $claimStartupWarning $state $true
    if ($null -ne $elapsed) {
        & $reportWarning "warning: job helper startup is still running after 10 seconds; attempt=$($state.ProcessId)/0; elapsed_ms=$elapsed"
    }
}.GetNewClosure()
# Before this process ends, on every path, the startup watcher, the lines
# written aside and the supervisor's queued warnings together get at most one
# second to finish writing.
$startupWatcher = $null
function Complete-Warnings {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $left = { [int][Math]::Max(0, 1000 - $clock.ElapsedMilliseconds) }
    foreach ($write in @(@($script:startupWatcher) + $script:asideWrites)) {
        if (-not $write) { continue }
        try {
            # Disposing a pipeline that is still running waits for it, so an
            # unfinished writer is left to end with the process.
            if ($write.Pending.AsyncWaitHandle.WaitOne((& $left))) { $write.Writer.Dispose() }
        } catch { }
    }
    try { [LauncherProcessJob]::FlushWarnings((& $left)) } catch { }
}
function Stop-StartupWatch { & $settleStartup $startup }
function Start-StartupWatch {
    # Watching lets the settlement name a slow startup as it ends, even when
    # the watcher below could not start: observation never stops the helper.
    $startup.Watching = $true
    try {
        $watcher = [PowerShell]::Create()
        [void]$watcher.AddScript({
            param($state, $claim, $warn)
            # Script blocks from the parent runspace run here as plain text.
            $claim = [ScriptBlock]::Create($claim)
            $warn = [ScriptBlock]::Create($warn)
            while (-not $state.Done) {
                $elapsed = & $claim $state $false
                if ($null -ne $elapsed) { & $warn $state $elapsed; return }
                if ($state.Warned) { return }
                Start-Sleep -Milliseconds 100
            }
        }).AddArgument($startup).AddArgument($claimStartupWarning.ToString()).AddArgument($warnStartup.ToString())
        $script:startupWatcher = @{ Writer = $watcher; Pending = $watcher.BeginInvoke() }
    } catch { }
}
# The supervisor's receipt publication, when it returns after ten seconds, is
# named once in the stage log, in the launcher's warning format.
function Write-SlowStepWarning([string]$Operation, [Diagnostics.Stopwatch]$Clock) {
    if ($request.ownedByParent -or $Clock.ElapsedMilliseconds -lt 10000) { return }
    & $reportWarning "warning: $Operation is still running after 10 seconds; attempt=$PID/0; elapsed_ms=$($Clock.ElapsedMilliseconds)"
}
try {
    $request = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($Spec)) | ConvertFrom-Json
    if (-not $request.ownedByParent) { Start-StartupWatch }
    Add-Type -Path $request.source
    $runInvoked = $true
    $outcome = [LauncherProcessJob]::Run($request.command, [string[]]$request.args, $request.cwd,
        [string[]]$request.environment, [Action]({ & $settleStartup $startup }.GetNewClosure()),
        [bool]$request.ownedByParent)
} catch {
    Stop-StartupWatch
    [Console]::Error.WriteLine($_.Exception.ToString())
    # Only a failure before invoking the native supervisor is a definite
    # startup refusal. An unexpected exception after that remains uncertain.
    if (!$request -or $runInvoked) { Complete-Warnings; exit 1 }
    $outcome = @{ ExitCode = 1; Stopped = $false; CleanupConfirmed = $true;
        ActiveProcesses = 0; Phase = 'helper'; Error = $_.Exception.Message }
}
Stop-StartupWatch
try {
    # The parent accepts cleanup only from this invocation's exclusive receipt.
    $json = $outcome | ConvertTo-Json -Compress
    $bytes = [Text.Encoding]::UTF8.GetBytes($json)
    $publication = [Diagnostics.Stopwatch]::StartNew()
    try {
        $stream = [IO.File]::Open($request.receipt, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
        try { $stream.Write($bytes, 0, $bytes.Length) } finally { $stream.Dispose() }
    } finally { Write-SlowStepWarning 'job helper receipt write' $publication }
    Complete-Warnings
    exit $outcome.ExitCode
} catch {
    [Console]::Error.WriteLine($_.Exception.ToString())
    Complete-Warnings
    exit 1
}
