param(
    [Parameter(Mandatory)][ValidateSet('x64', 'arm64')][string]$Architecture,
    [string]$Bundle = 'dist/windows-development'
)
$ErrorActionPreference = 'Stop'
$hostArchitecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
if ($hostArchitecture -ne $Architecture) { throw "Expected a native $Architecture runner" }
$expectedMachine = @{ x64 = 0x8664; arm64 = 0xaa64 }[$Architecture]
$programs = @(
    'hypermesh-native-desktop.exe', 'hypermesh_visor.dll', 'coreclr.dll',
    'hypermesh-visor.exe', 'hypermesh.exe', 'conductor.exe',
    'hypermesh-desktop-mcp.exe', 'hypermesh-acp.exe',
    'crash-sdk/bin/sentry.dll', 'crash-sdk/bin/crashpad_handler.exe',
    'crash-sdk/bin/hypermesh-crash-retention.exe'
)
foreach ($program in $programs) {
    $path = Join-Path $Bundle $program
    $reader = [IO.BinaryReader]::new([IO.File]::OpenRead($path))
    try {
        if ($reader.ReadUInt16() -ne 0x5a4d) { throw "Invalid executable header: $program" }
        $reader.BaseStream.Position = 0x3c
        $reader.BaseStream.Position = $reader.ReadUInt32()
        if ($reader.ReadUInt32() -ne 0x4550 -or $reader.ReadUInt16() -ne $expectedMachine) {
            throw "Expected a $Architecture PE executable: $program"
        }
    } finally { $reader.Dispose() }
}
