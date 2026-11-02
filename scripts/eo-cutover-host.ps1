param(
    [Parameter(Mandatory)][ValidateSet('SecureDirectory', 'Consumers', 'RehearsalConsumers')][string]$Action,
    [Parameter(Mandatory)][string]$Target,
    [int]$EnginePid = 0
)
$ErrorActionPreference = 'Stop'
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
    if ($Action -eq 'Consumers') { [Console]::WriteLine($processes.Count); exit 0 }
    # Rehearsal uses a fresh isolated home. Inspect command lines only in memory;
    # never expose them or exclude a process whose scope cannot be established.
    $count = 0
    foreach ($item in $processes) {
        if (-not $item.CommandLine) { throw 'process_scope_unavailable' }
        $text = $item.CommandLine.Replace([char]47, [char]92)
        $prefix = $Target.Replace([char]47, [char]92)
        if ($text.IndexOf($prefix, [StringComparison]::OrdinalIgnoreCase) -ge 0) { $count++ }
    }
    [Console]::WriteLine($count)
    exit 0
} catch {
    [Console]::Error.WriteLine('{"code":"host_inspection_failed"}')
    exit 1
}
