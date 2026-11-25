# Run only in an approved offline migration window. Rehearsal is the default.
[CmdletBinding(DefaultParameterSetName = 'Plan')]
param(
    [Parameter(Mandatory, ParameterSetName = 'Plan')][string]$Plan,
    [Parameter(ParameterSetName = 'Plan')][string]$WorkDirectory,
    [Parameter(ParameterSetName = 'Plan')][switch]$Apply,
    [Parameter(Mandatory, ParameterSetName = 'Rollback')][string]$RollbackJournal,
    [Parameter(Mandatory, ParameterSetName = 'Recover')][string]$RecoverJournal
)
$ErrorActionPreference = 'Stop'
# Reading the plan or journal is one call; when it took ten seconds or more it
# is named once as it returns, in the cutover's warning format. The line is
# written from its own runspace, so the cutover goes on without waiting for
# stderr to take it. Observation never blocks the cutover: when no runspace
# can be created for the line, it is not written.
function Read-CutoverInput([string]$Path) {
    $clock = [Diagnostics.Stopwatch]::StartNew()
    try { return Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json -AsHashtable }
    finally {
        if ($clock.ElapsedMilliseconds -ge 10000) {
            $line = "warning: cutover input read is still running after 10 seconds; attempt=$PID/0; elapsed_ms=$($clock.ElapsedMilliseconds)"
            try {
                $writer = [PowerShell]::Create()
                # Its own stderr stream, not the shared Console.Error writer,
                # so a slow write never holds the lock the wrapper's own
                # output needs.
                [void]$writer.AddScript({
                    param($text)
                    try {
                        $bytes = [Text.Encoding]::UTF8.GetBytes($text + [Environment]::NewLine)
                        $stream = [Console]::OpenStandardError()
                        try { $stream.Write($bytes, 0, $bytes.Length); $stream.Flush() } finally { $stream.Dispose() }
                    } catch { }
                }).AddArgument($line)
                $script:warningWrites += @(@{ Writer = $writer; Pending = $writer.BeginInvoke() })
            } catch { }
        }
    }
}
# Before the wrapper exits, on every path, its warning lines get at most a
# second to be written; the cutover work itself never waits for them.
$warningWrites = @()
function Complete-Warnings {
    $deadline = [Diagnostics.Stopwatch]::StartNew()
    foreach ($write in $script:warningWrites) {
        try {
            $left = [Math]::Max(0, 1000 - $deadline.ElapsedMilliseconds)
            # Disposing a pipeline that is still running waits for it, so an
            # unfinished writer is left to end with the process.
            if ($write.Pending.AsyncWaitHandle.WaitOne([int]$left)) { $write.Writer.Dispose() }
        } catch { }
    }
}
$mutex = $null
$ownsMutex = $false
$priorWrapper = $env:ENGRAM_CUTOVER_WRAPPER
try {
    if (-not $IsWindows) { throw 'windows_required' }
    $repository = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
    $entry = Join-Path $PSScriptRoot 'eo-cutover.mjs'
    if ($PSCmdlet.ParameterSetName -eq 'Plan') {
        $inputPath = [IO.Path]::GetFullPath($Plan)
        $configuration = Read-CutoverInput $inputPath
        if (-not $WorkDirectory) { $WorkDirectory = Join-Path $repository ('target/e-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)) }
        $WorkDirectory = [IO.Path]::GetFullPath($WorkDirectory)
        $scopePath = if ($Apply) { $configuration.home } else { Join-Path $WorkDirectory 'rehearsal/home' }
        $arguments = @($entry, '--plan', $inputPath, '--work-directory', $WorkDirectory)
        if ($Apply) { $arguments += '--apply' }
    } else {
        $inputPath = [IO.Path]::GetFullPath($(if ($RollbackJournal) { $RollbackJournal } else { $RecoverJournal }))
        $configuration = Read-CutoverInput $inputPath
        $scopePath = $configuration.home
        $arguments = @($entry, $(if ($RollbackJournal) { '--rollback' } else { '--recover' }), $inputPath)
    }
    if (-not [IO.Path]::IsPathFullyQualified($scopePath)) { throw 'scope_required' }
    # The named kernel mutex survives no process and serializes all supported
    # activation/recovery invocations, including invocations from another login.
    $identity = [Text.Encoding]::UTF8.GetBytes([IO.Path]::GetFullPath($scopePath).ToUpperInvariant())
    $digest = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($identity))
    $mutex = [Threading.Mutex]::new($false, ('Global\EngramOfflineCutover-' + $digest))
    try { $ownsMutex = $mutex.WaitOne(0) } catch [Threading.AbandonedMutexException] { $ownsMutex = $true }
    if (-not $ownsMutex) { throw 'cutover_process_active' }
    $env:ENGRAM_CUTOVER_WRAPPER = '1'
    & node @arguments
    $resultCode = $LASTEXITCODE
} catch {
    # Exception messages can contain private input or paths. Keep failures fixed.
    [Console]::Error.WriteLine('{"code":"cutover_entry_failed","stage":"preflight"}')
    $resultCode = 1
} finally {
    $env:ENGRAM_CUTOVER_WRAPPER = $priorWrapper
    if ($ownsMutex) { $mutex.ReleaseMutex() }
    if ($mutex) { $mutex.Dispose() }
    Complete-Warnings
}
exit $resultCode
