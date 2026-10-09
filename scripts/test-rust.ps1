$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# Windows entry point for the Rust test gate. Unix hosts use test-rust.sh,
# which additionally raises the inherited file-descriptor soft limit.

$repoRoot = Split-Path -Parent $PSScriptRoot
$cargoArgs = @($args)
$phases = @("ordinary", "scale-claims", "scale-roots", "scale-planning")
if ($cargoArgs.Count -gt 0) {
    if ($cargoArgs[0] -ceq "--phase") {
        if ($cargoArgs.Count -ne 2 -or $cargoArgs[1] -cnotin $phases) {
            [Console]::Error.WriteLine("Usage: test-rust.ps1 --phase ordinary|scale-claims|scale-roots|scale-planning")
            exit 2
        }
        $phases = @($cargoArgs[1])
        $cargoArgs = @()
    } else {
        $phases = @("ordinary")
    }
}
$previousTestThreads = $env:RUST_TEST_THREADS

# Eight threads measured fastest of 4, 8, 12 and 24 on a 24-core host; see
# docs/development.md. Use fewer where fewer processors are available.
$testThreads = if ($env:ENGRAM_TEST_THREADS) {
    $env:ENGRAM_TEST_THREADS
} elseif ($env:RUST_TEST_THREADS) {
    $env:RUST_TEST_THREADS
} else {
    [string][Math]::Min(8, [Environment]::ProcessorCount)
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

    foreach ($phase in $phases) {
        switch ($phase) {
            "ordinary" {
                & node scripts/test-temp.mjs -- cargo test @cargoArgs
            }
            "scale-claims" {
                Write-Output "Rust scale gate: claim-validated mutation decode budgets"
                & node scripts/test-temp.mjs -- cargo test claim_validated_mutations_are_bounded_at_project_scale -- --ignored --nocapture
            }
            "scale-roots" {
                Write-Output "Rust scale gate: root delta write bounds and historical cost measurements"
                # Intentionally include future ignored tests in the root_delta_scale_ family (substring filter).
                & node scripts/test-temp.mjs -- cargo test root_delta_scale_ -- --ignored --nocapture
            }
            "scale-planning" {
                Write-Output "Rust scale gate: planning bounds reached one mutation at a time"
                # Intentionally include future ignored tests in the planning_scale_ family (substring filter).
                & node scripts/test-temp.mjs -- cargo test planning_scale_ -- --ignored --nocapture
            }
        }
        if ($LASTEXITCODE -ne 0) {
            exit $LASTEXITCODE
        }
    }
} finally {
    $env:RUST_TEST_THREADS = $previousTestThreads
    Pop-Location
}
