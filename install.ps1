# Install the `theta` CLI on Windows.
#
#   irm https://thetabase.co/install.ps1 | iex
#
# The counterpart to `install.sh`, which refuses on Windows and says "download
# the .zip from Releases". That instruction stops one step short of a working
# command: it does not say where to put the extracted files or how to get them
# onto PATH, so following it exactly leaves you with `theta : The term 'theta'
# is not recognized`. That is what this script exists to fix.
#
# It keeps the same decisions `install.sh` made, for the same reasons:
#
# * **Per-user, never machine-wide.** No administrator prompt, nothing written
#   outside the user profile. A script piped from the internet into a shell has
#   already asked for a lot of trust.
# * **The checksum is verified**, and if one cannot be fetched it says so out
#   loud rather than skipping quietly. Somebody piping this into a shell should
#   know which of the two happened.
# * **Each binary is written to a temporary name and moved into place**, so an
#   interrupted install cannot leave a half-written file where a working one
#   used to be.
#
# It differs on one point, and deliberately. `install.sh` will not edit your
# shell profile; it prints the `export PATH=...` line and leaves it to you. On
# Windows the equivalent is not a line in a file, it is a registry value under
# HKCU\Environment, and telling somebody to edit the registry by hand is worse
# advice than editing it for them. So this updates the *user* PATH, prints
# exactly what it changed, and never touches the machine PATH.

$ErrorActionPreference = 'Stop'

$repo = if ($env:THETA_REPO) { $env:THETA_REPO } else { 'ThetaBase/thetabase' }
$version = if ($env:THETA_VERSION) { $env:THETA_VERSION } else { 'latest' }
$installDir = if ($env:THETA_INSTALL_DIR) {
    $env:THETA_INSTALL_DIR
} else {
    Join-Path $env:LOCALAPPDATA 'Programs\ThetaBase\bin'
}

function Say($text) { Write-Host $text }
function Die($text) { Write-Host "error: $text" -ForegroundColor Red; exit 1 }

# ---- which architecture -----------------------------------------------------

# Told plainly rather than guessed at. A wrong binary that downloads and then
# will not run is worse than a clear refusal.
$arch = switch ($env:PROCESSOR_ARCHITECTURE) {
    'AMD64' { 'x86_64' }
    'ARM64' { Die 'the release does not yet publish a Windows arm64 binary. x86_64 runs under emulation; download that archive by hand if you want to try it.' }
    default { Die "unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
}
$target = "$arch-pc-windows-msvc"

# ---- which version ----------------------------------------------------------

if ($version -eq 'latest') {
    try {
        $release = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest"
        $version = $release.tag_name
    } catch {
        Die "could not reach the GitHub release API: $($_.Exception.Message)"
    }
    if (-not $version) { Die 'could not determine the latest version; set THETA_VERSION' }
}

$archive = "theta-$version-$target.zip"
$url = "https://github.com/$repo/releases/download/$version/$archive"

Say "Installing theta $version ($target)"

# ---- download and verify ----------------------------------------------------

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("theta-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    $zip = Join-Path $tmp $archive
    try {
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
    } catch {
        Die "could not download $url : $($_.Exception.Message)"
    }

    $verified = $false
    try {
        $response = Invoke-WebRequest -Uri "$url.sha256" -UseBasicParsing
        # `.Content` is a byte array in Windows PowerShell 5.1 for anything it
        # does not recognise as text, and `-split` on a byte array silently
        # stringifies it -- so the first "word" came back as `98`, the decimal
        # value of the letter `b`. Decoded explicitly rather than trusted to be
        # a string, and the type is checked because PowerShell 7 hands back a
        # string here and would be mangled by an unconditional decode.
        $text = if ($response.Content -is [byte[]]) {
            [System.Text.Encoding]::UTF8.GetString($response.Content)
        } else {
            [string]$response.Content
        }
        $expected = ($text.Trim() -split '\s+')[0]
    } catch {
        $expected = $null
    }

    # A checksum that is not 64 hex characters is not a checksum, and comparing
    # against one would either pass by accident or fail for the wrong reason --
    # which is exactly what happened the first time this ran.
    if ($expected -and $expected -notmatch '^[0-9a-fA-F]{64}$') {
        Die "the published checksum for this release is not a SHA-256 digest: '$expected'"
    }

    if ($expected) {
        $actual = (Get-FileHash -Path $zip -Algorithm SHA256).Hash
        if ($actual -ine $expected) {
            Die "checksum mismatch -- the download is not what was published`n  expected $expected`n  actual   $actual"
        }
        $verified = $true
    } else {
        Say '  warning: no published checksum for this release, so nothing was verified'
    }

    # ---- install ------------------------------------------------------------

    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    $unpacked = Join-Path $tmp "theta-$version-$target"
    if (-not (Test-Path (Join-Path $unpacked 'theta.exe'))) {
        Die 'the archive did not contain a `theta.exe`'
    }

    New-Item -ItemType Directory -Path $installDir -Force | Out-Null

    foreach ($name in 'theta.exe', 'theta-mcp.exe') {
        $source = Join-Path $unpacked $name
        if (-not (Test-Path $source)) { continue }

        $final = Join-Path $installDir $name
        $staged = "$final.tmp"
        Copy-Item $source $staged -Force
        # `Move-Item -Force` over a running executable fails on Windows rather
        # than succeeding and surprising you later, which is the right way
        # round: an upgrade while the MCP server is running should say so.
        try {
            Move-Item $staged $final -Force
        } catch {
            Remove-Item $staged -Force -ErrorAction SilentlyContinue
            Die "could not replace $final -- is it running? Close it and run this again."
        }
        Say "Installed $final"
    }

    if ($verified) { Say '  checksum verified' }

    # ---- PATH ---------------------------------------------------------------

    # The *user* PATH, read from the registry rather than from `$env:PATH`.
    # `$env:PATH` is the current process's copy, which includes the machine PATH
    # and anything the session added -- writing that back would copy machine
    # entries into the user's own PATH permanently.
    $userPath = [Environment]::GetEnvironmentVariable('PATH', 'User')
    $entries = @()
    if ($userPath) { $entries = $userPath -split ';' | Where-Object { $_ } }

    if ($entries -notcontains $installDir) {
        $updated = (@($entries) + $installDir) -join ';'
        [Environment]::SetEnvironmentVariable('PATH', $updated, 'User')
        # And for this session, so the next line of the quickstart works without
        # opening a new terminal.
        $env:PATH = "$env:PATH;$installDir"
        Say ''
        Say "Added $installDir to your user PATH."
        Say 'Open a new terminal for it to take effect everywhere.'
    } else {
        Say ''
        Say "$installDir is already on your PATH."
    }

    Say ''
    Say 'Try it:'
    Say '  theta --version'
    Say '  theta login'
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}
