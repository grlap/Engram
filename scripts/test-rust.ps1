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
$previousBuildJobs = $env:CARGO_BUILD_JOBS

# Keep Windows commit demand bounded. Cargo and libtest still receive any
# explicit command-line options through the ordinary phase's passthrough.
$buildJobs = if ($previousBuildJobs) { $previousBuildJobs } else { "2" }
$testThreads = if ($env:ENGRAM_TEST_THREADS) {
    $env:ENGRAM_TEST_THREADS
} elseif ($env:RUST_TEST_THREADS) {
    $env:RUST_TEST_THREADS
} else {
    [string][Math]::Min(4, [Environment]::ProcessorCount)
}

$jobsOverride = $null
$hasCargoConfig = $false
$hasPossibleClusteredJobs = $false
$reportedTestThreads = $testThreads
$inTestArgs = $false
for ($i = 0; $i -lt $cargoArgs.Count; $i++) {
    $arg = $cargoArgs[$i]
    if (-not $inTestArgs) {
        if ($arg -ceq "--") {
            $inTestArgs = $true
        } elseif ($arg -cin @("-j", "--jobs") -and $i + 1 -lt $cargoArgs.Count) {
            $jobsOverride = $cargoArgs[++$i]
        } elseif ($arg -cmatch '^(?:-j=?|--jobs=)(.+)$') {
            $jobsOverride = $Matches[1]
        } elseif ($arg -ceq "--config" -and $i + 1 -lt $cargoArgs.Count) {
            $hasCargoConfig = $true
            $i++
        } elseif ($arg.StartsWith("--config=", [StringComparison]::Ordinal)) {
            $hasCargoConfig = $true
        } elseif ($arg -cmatch '^-[a-zA-Z]+j') {
            # A short-option cluster can contain -j, but Cargo knows which
            # preceding options consume a value.
            $hasPossibleClusteredJobs = $true
        }
    } elseif ($arg -ceq "--") {
        break
    } elseif ($arg -ceq "--test-threads" -and $i + 1 -lt $cargoArgs.Count) {
        $reportedTestThreads = $cargoArgs[++$i]
    } elseif ($arg -cmatch '^--test-threads=(.+)$') {
        $reportedTestThreads = $Matches[1]
    }
}

$selectedBuildJobs = if ($jobsOverride) { $jobsOverride } else { $buildJobs }
$reportedBuildJobs = if ($hasPossibleClusteredJobs) {
    "resolved by Cargo (short options; CARGO_BUILD_JOBS=$buildJobs)"
} elseif ($hasCargoConfig -and -not $jobsOverride) {
    "resolved by Cargo (--config; CARGO_BUILD_JOBS=$buildJobs)"
} elseif ($selectedBuildJobs -cnotmatch '^[1-9][0-9]*$') {
    "resolved by Cargo (jobs setting=$selectedBuildJobs)"
} else {
    $selectedBuildJobs
}

$parsedTestThreads = 0
if (-not [int]::TryParse($testThreads, [ref]$parsedTestThreads) -or $parsedTestThreads -le 0) {
    [Console]::Error.WriteLine("The test thread count must be a positive integer; got '$testThreads'.")
    exit 2
}

Push-Location $repoRoot
try {
    $env:CARGO_BUILD_JOBS = $buildJobs
    $env:RUST_TEST_THREADS = $testThreads
    Write-Output "Rust test gate: fd soft limit=n/a (Windows), build jobs=$reportedBuildJobs, test threads=$reportedTestThreads"

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
    $env:CARGO_BUILD_JOBS = $previousBuildJobs
    $env:RUST_TEST_THREADS = $previousTestThreads
    Pop-Location
}
