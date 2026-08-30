$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# Windows entry point for the Rust test gate. Unix hosts use test-rust.sh,
# which additionally raises the inherited file-descriptor soft limit.

$repoRoot = Split-Path -Parent $PSScriptRoot
$cargoArgs = @($args)
$previousTestThreads = $env:RUST_TEST_THREADS

$testThreads = if ($env:ENGRAM_TEST_THREADS) {
    $env:ENGRAM_TEST_THREADS
} elseif ($env:RUST_TEST_THREADS) {
    $env:RUST_TEST_THREADS
} else {
    "4"
}

$parsedTestThreads = 0
if (-not [int]::TryParse($testThreads, [ref]$parsedTestThreads) -or $parsedTestThreads -le 0) {
    Write-Error "ENGRAM_TEST_THREADS must be a positive integer; got '$testThreads'."
    exit 2
}

Push-Location $repoRoot
try {
    $env:RUST_TEST_THREADS = $testThreads
    Write-Output "Rust test gate: fd soft limit=n/a (Windows), test threads=$testThreads"

    & cargo test @cargoArgs
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }

    if ($cargoArgs.Count -eq 0) {
        Write-Output "Rust performance gate: claim-validated mutations, test threads=1"
        & cargo test claim_validated_mutations_are_bounded_at_project_scale -- --ignored --nocapture --test-threads=1
        if ($LASTEXITCODE -ne 0) {
            exit $LASTEXITCODE
        }
    }
} finally {
    $env:RUST_TEST_THREADS = $previousTestThreads
    Pop-Location
}
