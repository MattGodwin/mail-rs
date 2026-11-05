param(
    [Parameter(Mandatory = $true)]
    [string]$Executable
)

$ErrorActionPreference = 'Stop'
$exePath = (Resolve-Path -LiteralPath $Executable).Path
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$dumpbin = & $vswhere -latest -products * -find 'VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe' | Select-Object -First 1
if (-not $dumpbin) { throw 'Could not find dumpbin.exe to verify runtime dependencies' }
$deps = & $dumpbin /dependents $exePath
if ($LASTEXITCODE -ne 0) { throw "dumpbin failed with exit code $LASTEXITCODE" }
$deps
if ($deps -match '(?i)(VCRUNTIME|MSVCP|MSVCR|ucrtbased).*\.dll') {
    throw 'Executable depends on the Visual C++ runtime; check the static CRT build flags'
}
