# Installs or updates FlowBar from the latest release, for the current user:
#   irm https://github.com/best-shuke/FlowBar/releases/latest/download/install.ps1 | iex
# Runs the release's Setup program silently. Downloads by PowerShell carry no
# mark of the web, so SmartScreen does not stop it.
# ASCII only: Windows PowerShell 5.1 decodes a download without a charset as Latin-1.
& {
    $ErrorActionPreference = 'Stop'
    # Windows PowerShell 5.1 renders the progress bar slowly enough to stall downloads.
    $ProgressPreference = 'SilentlyContinue'
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    # The registry has the machine's own architecture, also under x64 emulation on ARM64.
    $machine = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment').PROCESSOR_ARCHITECTURE
    $arch = switch ($machine) {
        'AMD64' { 'x86_64' }
        'ARM64' { 'arm64' }
        default { throw "FlowBar does not support $machine Windows." }
    }

    $release = Invoke-RestMethod -UseBasicParsing 'https://api.github.com/repos/best-shuke/FlowBar/releases/latest'
    $version = $release.tag_name.TrimStart('v')
    $name = "FlowBar-$version-windows-$arch-setup.exe"
    $asset = $release.assets | Where-Object name -eq $name
    if (-not $asset) { throw "Release $($release.tag_name) has no $name." }

    $setup = Join-Path ([IO.Path]::GetTempPath()) $name
    try {
        Write-Host "Downloading FlowBar $version ($arch)..."
        Invoke-WebRequest -UseBasicParsing $asset.browser_download_url -OutFile $setup
        if ($asset.digest -like 'sha256:*') {
            $hash = (Get-FileHash -Algorithm SHA256 $setup).Hash
            if ($hash -ne $asset.digest.Substring(7)) { throw 'The download is damaged; please try again.' }
        }
        Write-Host 'Installing...'
        $installer = Start-Process -FilePath $setup -ArgumentList '/S' -Wait -PassThru
        if ($installer.ExitCode) { throw "Setup failed with exit code $($installer.ExitCode)." }
    } finally {
        Remove-Item -Force $setup -ErrorAction SilentlyContinue
    }

    $directory = (Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\FlowBar').InstallLocation
    Write-Host "FlowBar $version is installed in $directory and on the Start menu."
    Start-Process (Join-Path $directory 'FlowBar.exe')
}
