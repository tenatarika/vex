# Install vex and vex-mcp on Windows from GitHub Releases.
#
#   irm https://raw.githubusercontent.com/tenatarika/vex/main/install.ps1 | iex
#
# Environment:
#   VEX_VERSION      release to install, e.g. 1.28.0 (default: latest)
#   VEX_INSTALL_DIR  where the binaries go (default: %LOCALAPPDATA%\Programs\vex)
#   VEX_NO_MCP=1     skip vex-mcp (the MCP server)
#
# The install directory is added to the user PATH. vex.exe ships with
# DirectML.dll beside it for GPU indexing; both are installed.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'  # Invoke-WebRequest is very slow with the progress bar on PowerShell 5.1

function Install-Vex {
    $repo = 'https://github.com/tenatarika/vex'
    $target = 'x86_64-pc-windows-msvc'
    $installDir = if ($env:VEX_INSTALL_DIR) { $env:VEX_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\vex' }

    switch ($env:PROCESSOR_ARCHITECTURE) {
        'AMD64' { }
        'ARM64' { Write-Host 'vex-install: no ARM64 build; installing the x64 build, which Windows 11 runs under emulation' }
        default { throw "vex-install: no prebuilt vex for $($env:PROCESSOR_ARCHITECTURE); build from source: cargo install vex-search --locked" }
    }
    # System32's bsdtar, not a Git-for-Windows GNU tar earlier on PATH, which
    # reads `C:\...` in -C as a remote host.
    $tar = Join-Path $env:SystemRoot 'System32\tar.exe'
    if (-not (Test-Path $tar)) {
        throw 'vex-install: tar.exe not found (it ships with Windows 10 1803 and later)'
    }

    $base = if ($env:VEX_VERSION) { "$repo/releases/download/v$($env:VEX_VERSION.TrimStart('v'))" } else { "$repo/releases/latest/download" }
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $names = @('vex')
    if ($env:VEX_NO_MCP -ne '1') { $names += 'vex-mcp' }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("vex-install-" + [guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        New-Item -ItemType Directory -Force -Path $installDir | Out-Null
        foreach ($name in $names) {
            $archive = "$name-$target.tar.gz"
            Write-Host "vex-install: downloading $archive"
            Invoke-WebRequest -UseBasicParsing -Uri "$base/$archive" -OutFile (Join-Path $tmp $archive)
            $unpacked = Join-Path $tmp $name
            New-Item -ItemType Directory -Path $unpacked | Out-Null
            & $tar -xzf (Join-Path $tmp $archive) -C $unpacked
            if ($LASTEXITCODE -ne 0) { throw "vex-install: could not unpack $archive" }
            if (-not (Test-Path (Join-Path $unpacked "$name.exe"))) { throw "vex-install: $archive has no $name.exe" }
            Copy-Item -Force -Path (Join-Path $unpacked '*') -Destination $installDir
        }
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries = @($userPath -split ';' | Where-Object { $_ })
    if ($entries -notcontains $installDir) {
        [Environment]::SetEnvironmentVariable('Path', (($entries + $installDir) -join ';'), 'User')
        Write-Host "vex-install: added $installDir to your user PATH (open a new terminal to pick it up)"
    }
    if (($env:Path -split ';') -notcontains $installDir) { $env:Path = "$env:Path;$installDir" }

    $version = & (Join-Path $installDir 'vex.exe') --version
    Write-Host "vex-install: installed $version to $installDir"
    Write-Host 'vex-install: next: cd <your repo>; vex index'
}

Install-Vex
