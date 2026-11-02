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
$mutex = $null
$ownsMutex = $false
$priorWrapper = $env:ENGRAM_CUTOVER_WRAPPER
try {
    if (-not $IsWindows) { throw 'windows_required' }
    $repository = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
    $entry = Join-Path $PSScriptRoot 'eo-cutover.mjs'
    if ($PSCmdlet.ParameterSetName -eq 'Plan') {
        $inputPath = [IO.Path]::GetFullPath($Plan)
        $configuration = Get-Content -LiteralPath $inputPath -Raw | ConvertFrom-Json -AsHashtable
        if (-not $WorkDirectory) { $WorkDirectory = Join-Path $repository ('target/e-' + [Guid]::NewGuid().ToString('N').Substring(0, 8)) }
        $WorkDirectory = [IO.Path]::GetFullPath($WorkDirectory)
        $scopePath = if ($Apply) { $configuration.home } else { Join-Path $WorkDirectory 'rehearsal/home' }
        $arguments = @($entry, '--plan', $inputPath, '--work-directory', $WorkDirectory)
        if ($Apply) { $arguments += '--apply' }
    } else {
        $inputPath = [IO.Path]::GetFullPath($(if ($RollbackJournal) { $RollbackJournal } else { $RecoverJournal }))
        $configuration = Get-Content -LiteralPath $inputPath -Raw | ConvertFrom-Json -AsHashtable
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
}
exit $resultCode
