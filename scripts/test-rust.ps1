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
                if ($LASTEXITCODE -eq 0 -and $cargoArgs.Count -eq 0) {
                    # The real-HTTPS data-branch workflows are ignored in the run above so
                    # that their supervised children never share the host with the rest of
                    # the suite; this pass runs exactly those, one child at a time.
                    Write-Output "Rust isolated gate: Windows HTTPS data-branch workflows"
                    # A filter that matches nothing would pass with "0 passed"; count what ran.
                    $isolatedPassed = 0
                    & node scripts/test-temp.mjs -- cargo test --lib data_branch::task_exchange_windows_ -- --ignored --nocapture | ForEach-Object {
                        if ($_ -match '^test result: \S+\. (\d+) passed') { $isolatedPassed += [int]$Matches[1] }
                        $_
                    }
                    if ($LASTEXITCODE -eq 0 -and $isolatedPassed -eq 0) {
                        [Console]::Error.WriteLine("Rust isolated gate selected no tests: the data_branch::task_exchange_windows_ filter matched nothing")
                        exit 1
                    }
                } elseif ($LASTEXITCODE -eq 0 -and $cargoArgs.Count -gt 0) {
                    Write-Output "Rust isolated gate skipped: Cargo arguments select the tests; the Windows HTTPS data-branch workflows are ignored unless selected with --ignored"
                }
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
