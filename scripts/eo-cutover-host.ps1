param(
    [Parameter(Mandatory)][ValidateSet('SecureDirectory', 'Consumers', 'RehearsalConsumers')][string]$Action,
    [Parameter(Mandatory)][string]$Target,
    [int]$EnginePid = 0
)
$ErrorActionPreference = 'Stop'
function ConvertTo-RehearsalPath([string]$Value) {
    if ($Value.StartsWith('\\?\UNC\', [StringComparison]::OrdinalIgnoreCase)) {
        $Value = '\\' + $Value.Substring(8)
    } elseif ($Value -match '^\\\\\?\\[A-Z]:\\') {
        $Value = $Value.Substring(4)
    } elseif ($Value -match '^\\\\[?.]\\') {
        return
    }
    if (-not [IO.Path]::IsPathFullyQualified($Value)) { return }
    return [IO.Path]::TrimEndingDirectorySeparator([IO.Path]::GetFullPath($Value))
}

function Get-ProcessArguments([string]$CommandLine) {
    # Windows separates argv on unquoted spaces and tabs. Other whitespace
    # can be part of a path, and an open quote runs to the end of the line.
    $tokens = [regex]'(?<![^ \t])(?:"[^"]*(?:"|$)|[^" \t])+(?=[ \t]|$)'
    foreach ($token in $tokens.Matches($CommandLine)) {
        $token.Value.Replace('"', '')
    }
}

function Get-EngramScope([string[]]$Argv) {
    $index = 1
    $selectedHome = $null
    while ($index -lt $Argv.Count) {
        $option, $value = $Argv[$index] -csplit '=', 2
        if ($option -ceq '--warn-slow') { $index++; continue }
        if ($option -cnotin @('--home', '--project-file', '--host-path-policy')) { break }
        if ($null -eq $value) { $value = $Argv[++$index] }
        if ($option -ceq '--home') { $selectedHome = $value }
        $index++
    }
    $command = $Argv[$index]
    if ($command -cin @('source', '--version', '-V', '--help', '-h')) { return }
    if ($command -ceq 'migration') {
        # Migration uses explicit files and does not resolve the active home.
        $index++
        while ($Argv[$index] -ceq '--warn-slow') { $index++ }
        $operation = $Argv[$index]
        $scopeOption = switch -CaseSensitive ($operation) {
            'export' { '--database' }
            'import' { '--out' }
            default { return }
        }
        for ($i = $index + 1; $i -lt $Argv.Count; $i++) {
            if ($Argv[$i] -ceq '--warn-slow') { continue }
            $option, $value = $Argv[$i] -csplit '=', 2
            if ($null -eq $value) { $value = $Argv[++$i] }
            if ($option -ceq $scopeOption) { return $value }
        }
        return
    }
    return $selectedHome
}

$rehearsalFailure = 'census_unavailable'
try {
    if (-not $IsWindows) { throw 'windows_required' }
    if ($Action -eq 'SecureDirectory') {
        $acl = [System.Security.AccessControl.DirectorySecurity]::new()
        $acl.SetAccessRuleProtection($true, $false)
        $inherit = [System.Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
        $propagate = [System.Security.AccessControl.PropagationFlags]::None
        $allow = [System.Security.AccessControl.AccessControlType]::Allow
        $sid = [System.Security.Principal.WindowsIdentity]::GetCurrent().User
        foreach ($identity in @($sid, [System.Security.Principal.SecurityIdentifier]::new('S-1-5-18'))) {
            $acl.AddAccessRule([System.Security.AccessControl.FileSystemAccessRule]::new($identity, 'FullControl', $inherit, $propagate, $allow))
        }
        Set-Acl -LiteralPath $Target -AclObject $acl
        exit 0
    }
    $census = @(Get-CimInstance Win32_Process)
    if ($Action -eq 'RehearsalConsumers') {
        $rehearsalFailure = 'scope_invalid'
        $root = ConvertTo-RehearsalPath $Target
        if (-not $root) { throw 'scope_required' }
        $rehearsalFailure = 'census_failed'
        $prefix = $root.TrimEnd('\') + '\'
        $count = 0
        foreach ($item in $census) {
            if ($item.ProcessId -eq $EnginePid -or -not $item.CommandLine -or
                $item.Name -notmatch '^(?:node(?:[.]exe)?$|engram(?:[-.]|$))') { continue }
            $isNode = $item.Name -match '^node(?:[.]exe)?$'
            if ($isNode -and $item.CommandLine -notmatch 'eo-cutover[.]mjs') { continue }
            $argv = @(Get-ProcessArguments $item.CommandLine)
            if ($isNode) {
                if ($argv[1] -notmatch '(?:^|[\\/])eo-cutover[.]mjs$') { continue }
                $scope = $null
                for ($i = 2; $i -lt $argv.Count; $i++) {
                    if ($argv[$i] -ceq '--apply') { continue }
                    $option = $argv[$i]
                    $value = $argv[++$i]
                    if ($option -cin @('--work-directory', '--recover', '--rollback')) { $scope = $value }
                }
            } else {
                $scope = Get-EngramScope $argv
            }
            if (-not $scope) { continue }
            $path = ConvertTo-RehearsalPath $scope
            if ($path -and ($path.Equals($root, [StringComparison]::OrdinalIgnoreCase) -or
                $path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase))) { $count++ }
        }
        [Console]::WriteLine($count)
        exit 0
    }
    if (@($census | Where-Object { $_.ProcessId -ne $EnginePid -and $_.Name -match '^node(?:[.]exe)?$' -and -not $_.CommandLine }).Count -gt 0) {
        throw 'process_scope_unavailable'
    }
    $engines = @($census | Where-Object {
        $_.ProcessId -ne $EnginePid -and $_.Name -match '^node(?:[.]exe)?$' -and
        $_.CommandLine -match '[\\/]eo-cutover[.]mjs(?:["\s]|$)'
    })
    # A wrapper may exit while its Node child continues. The engine census
    # keeps that orphan exclusive even after its wrapper's mutex is abandoned.
    if ($engines.Count -gt 0) { [Console]::WriteLine($engines.Count); exit 0 }
    $processes = @($census | Where-Object { $_.Name -match '^engram(?:[-.]|$)' })
    [Console]::WriteLine($processes.Count)
    exit 0
} catch {
    if ($Action -eq 'RehearsalConsumers') { [Console]::WriteLine($rehearsalFailure) }
    [Console]::Error.WriteLine('{"code":"host_inspection_failed"}')
    exit 1
}
