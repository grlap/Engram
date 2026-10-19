param([Parameter(Mandatory = $true)][string]$Spec)
$ErrorActionPreference = 'Stop'
$request = $null
$runInvoked = $false
try {
    $request = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($Spec)) | ConvertFrom-Json
    Add-Type -Path $request.source
    $runInvoked = $true
    $outcome = [LauncherProcessJob]::Run($request.command, [string[]]$request.args, $request.cwd,
        [string[]]$request.environment)
} catch {
    [Console]::Error.WriteLine($_.Exception.ToString())
    # Only a failure before invoking the native supervisor is a definite
    # startup refusal. An unexpected exception after that remains uncertain.
    if (!$request -or $runInvoked) { exit 1 }
    $outcome = @{ ExitCode = 1; Stopped = $false; CleanupConfirmed = $true;
        ActiveProcesses = 0; Phase = 'helper'; Error = $_.Exception.Message }
}
try {
    # The parent accepts cleanup only from this invocation's exclusive receipt.
    $json = $outcome | ConvertTo-Json -Compress
    $bytes = [Text.Encoding]::UTF8.GetBytes($json)
    $stream = [IO.File]::Open($request.receipt, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    try { $stream.Write($bytes, 0, $bytes.Length) } finally { $stream.Dispose() }
    exit $outcome.ExitCode
} catch {
    [Console]::Error.WriteLine($_.Exception.ToString())
    exit 1
}
