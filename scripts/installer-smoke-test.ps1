# Installs a built Setup program silently, installs it again over a running FlowBar,
# then uninstalls it. Installs into the current user's %LOCALAPPDATA%\Programs\FlowBar.
# Usage: .\scripts\installer-smoke-test.ps1 dist\FlowBar-<version>-windows-x86_64-setup.exe
param([Parameter(Mandatory)][string]$Setup)
$ErrorActionPreference = 'Stop'
# GitHub shows the logs only to maintainers, but annotations to everyone.
trap {
    if ($env:GITHUB_ACTIONS) { Write-Host "::error::$_" }
    break
}
$directory = Join-Path $env:LOCALAPPDATA 'Programs\FlowBar'
$executable = Join-Path $directory 'FlowBar.exe'
$shortcut = Join-Path ([Environment]::GetFolderPath('Programs')) 'FlowBar.lnk'
$entry = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\FlowBar'
$run = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'

function Check($condition, $message) {
    if (-not $condition) { throw "FAIL $message" }
    Write-Host "PASS $message"
}
function Install {
    $process = Start-Process $Setup -ArgumentList '/S' -Wait -PassThru
    Check ($process.ExitCode -eq 0) "Setup exits with 0, got $($process.ExitCode)"
}

Install
# PowerShell reads a GUI program's output only when it is piped on; otherwise it closes
# the pipe while FlowBar still writes to it.
Check ((& $executable --version | Out-String) -match '^FlowBar ') 'the installed FlowBar runs'
Check (Test-Path $shortcut) 'Setup adds a Start menu shortcut'
Check ((Get-ItemProperty $entry).InstallLocation -eq $directory) 'Setup registers its uninstaller'

$running = Start-Process $executable -PassThru
Start-Sleep 3
Check (-not $running.HasExited) 'FlowBar starts'
Install
Check $running.HasExited 'Setup ends a running FlowBar before replacing it'

# An uninstaller outside a FlowBar directory refuses to remove its directory. _?= runs
# it in place, so that -Wait covers the whole run.
$elsewhere = Join-Path ([IO.Path]::GetTempPath()) "FlowBar-guard-$([guid]::NewGuid())"
New-Item -ItemType Directory $elsewhere | Out-Null
Copy-Item (Join-Path $directory 'Uninstall.exe'), (Join-Path $directory 'rill.exe') $elsewhere
Start-Process (Join-Path $elsewhere 'Uninstall.exe') -ArgumentList '/S', "_?=$elsewhere" -Wait
Check (Test-Path (Join-Path $elsewhere 'rill.exe')) 'the uninstaller keeps a directory not named FlowBar'
Remove-Item -Recurse -Force $elsewhere

Set-ItemProperty $run FlowBar "`"$executable`""
Start-Process (Join-Path $directory 'Uninstall.exe') -ArgumentList '/S' -Wait
# The uninstaller continues from a copy of itself in the temporary directory.
for ($i = 0; $i -lt 100 -and (Test-Path $directory); $i++) { Start-Sleep -Milliseconds 300 }
Check (-not (Test-Path $directory)) 'uninstalling removes the install directory'
Check (-not (Test-Path $shortcut)) 'uninstalling removes the shortcut'
Check (-not (Test-Path $entry)) 'uninstalling removes the uninstall entry'
Check ($null -eq (Get-ItemProperty $run).FlowBar) 'uninstalling removes the login item'
