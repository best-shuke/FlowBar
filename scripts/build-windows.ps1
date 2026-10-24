# Builds dist\FlowBar: FlowBar.exe, its mihomo core and the license files, ready to zip.
# Usage: .\scripts\build-windows.ps1 -Core C:\path\mihomo.exe [-Target aarch64-pc-windows-msvc]
#        [-Geo <directory from scripts/fetch-geo.sh>]
param(
    [string]$Core,
    [string]$Target,
    [string]$Geo
)
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot)

if (-not $Core) {
    $found = Get-Command mihomo -ErrorAction SilentlyContinue
    if (-not $found) {
        throw '请指定 mihomo.exe 路径：.\scripts\build-windows.ps1 -Core C:\path\mihomo.exe'
    }
    $Core = $found.Source
}
$arguments = @('build', '--release', '--locked')
$output = 'target\release'
if ($Target) {
    $arguments += @('--target', $Target)
    $output = "target\$Target\release"
}
cargo @arguments
if ($LASTEXITCODE) { exit $LASTEXITCODE }

New-Item -ItemType Directory -Force dist | Out-Null
$staging = Join-Path dist ".pack-$([guid]::NewGuid())"
$app = Join-Path $staging 'FlowBar'
New-Item -ItemType Directory -Force $app | Out-Null
try {
    Copy-Item "$output\flowbar.exe" "$app\FlowBar.exe"
    Copy-Item $Core "$app\mihomo.exe"
    Copy-Item Resources\THIRD_PARTY.md, Resources\LICENSE-MIHOMO $app
    if ($Geo) { Copy-Item "$Geo\geoip.metadb", "$Geo\GeoSite.dat" $app }
    $licenseArguments = @('scripts\licenses.py', "$app\LICENSE-RUST-DEPENDENCIES.txt")
    if ($Target) { $licenseArguments += $Target }
    python @licenseArguments
    if ($LASTEXITCODE) { exit $LASTEXITCODE }
    # A cross-built executable cannot run here.
    $native = -not $Target -or ($Target -like 'aarch64*') -eq ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64')
    if ($native) { & "$app\FlowBar.exe" --version | Out-Host }
    if (Test-Path dist\FlowBar) { Remove-Item -Recurse -Force dist\FlowBar }
    Move-Item $app dist\FlowBar
} finally {
    Remove-Item -Recurse -Force $staging -ErrorAction SilentlyContinue
}
Write-Host "已生成：$((Resolve-Path dist\FlowBar).Path)"
