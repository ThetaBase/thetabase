# Bring up a development stack that GitHub can actually deliver to.
#
#   .\dev-stack.ps1
#
# Starts the control plane on 127.0.0.1:8080 and a Cloudflare quick tunnel in
# front of it, binds this repo to the seeded dev project, then prints the
# public webhook URL. Ctrl-C stops both.
#
# ASCII only, deliberately: Windows PowerShell 5.1 reads a BOM-less file as
# ANSI, and a stray non-ASCII character in a comment becomes a parse error.

$ErrorActionPreference = "Stop"
$repo = $PSScriptRoot
$secretsDir = Join-Path (Split-Path $repo -Parent) "secrets"
$secretFile = Join-Path $secretsDir "webhook-secret.txt"
$appJson = Join-Path $secretsDir "github-app.json"
$pem = Join-Path $secretsDir "github-app.pem"
$exe = Join-Path $repo "target\debug\theta-control.exe"

if (-not (Test-Path $exe)) {
    throw "$exe not built yet. Run: cargo build -p theta-control"
}
New-Item -ItemType Directory -Force -Path $secretsDir | Out-Null

# The App's own webhook secret wins. GitHub generates it during the manifest
# exchange and signs every delivery with it, so a locally-invented secret would
# make the control plane reject every real delivery as unsigned - which looks
# exactly like an attack, and is a confusing way to discover this.
if (Test-Path $appJson) {
    $secret = (Get-Content $appJson -Raw | ConvertFrom-Json).webhook_secret
    Write-Host "webhook secret: from the GitHub App (github-app.json)"
} elseif (Test-Path $secretFile) {
    $secret = (Get-Content $secretFile -Raw).Trim()
    Write-Host "webhook secret: local placeholder ($secretFile)"
} else {
    # RandomNumberGenerator::Fill is .NET Core only; 5.1 runs on .NET Framework.
    $bytes = New-Object byte[] 32
    $rng = New-Object System.Security.Cryptography.RNGCryptoServiceProvider
    try { $rng.GetBytes($bytes) } finally { $rng.Dispose() }
    $secret = ($bytes | ForEach-Object { $_.ToString("x2") }) -join ""
    $secret | Out-File -FilePath $secretFile -Encoding ascii -NoNewline
    icacls $secretFile /inheritance:r /grant:r "$($env:USERNAME):(R,W)" | Out-Null
    Write-Host "webhook secret: generated a placeholder (no App yet) -> $secretFile"
}

$env:THETA_GITHUB_WEBHOOK_SECRET = $secret
$env:THETA_CONTROL_LISTEN = "127.0.0.1:8080"
$env:THETA_INSTANCE_ROOT = Join-Path $secretsDir "instances"
$env:RUST_LOG = "theta_control=debug"

# Authenticate as the App so the Safety Layer's comment can be posted. Without
# these the webhook still verifies and branch actions still run; only the
# comment cannot be posted.
if ((Test-Path $appJson) -and (Test-Path $pem)) {
    $env:THETA_GITHUB_APP_ID = [string](Get-Content $appJson -Raw | ConvertFrom-Json).id
    $env:THETA_GITHUB_APP_KEY_PATH = $pem
    Write-Host "app auth: id $($env:THETA_GITHUB_APP_ID)"
} else {
    Write-Host "app auth: none yet - branching will work, comments will not post"
}

$planeLog = Join-Path $secretsDir "control-plane.log"
$planeErr = Join-Path $secretsDir "control-plane.err"

# Truncate rather than delete. Deleting needs DELETE on the file or
# DELETE_CHILD on the folder, and the secrets folder is deliberately locked
# down; truncating needs only write, which we already have.
function Reset-Log($path) {
    if (Test-Path $path) {
        try { Set-Content -Path $path -Value $null -Encoding ascii } catch { }
    }
}
Reset-Log $planeLog
Reset-Log $planeErr

# --dev-seed gives us org_local/scratch and prints an identity token, which the
# repo-to-project binding below needs. Local only, by construction.
# Refuse to start on top of something else. A second theta-control cannot bind
# 8080, exits immediately, and every step after it fails in a way that looks
# like a different problem - which is exactly how an hour gets lost.
$busy = Get-NetTCPConnection -LocalPort 8080 -State Listen -ErrorAction SilentlyContinue
if ($busy) {
    $owner = (Get-Process -Id $busy[0].OwningProcess -ErrorAction SilentlyContinue)
    throw "127.0.0.1:8080 is already in use by $($owner.Name) (pid $($owner.Id)). Stop it first."
}

$plane = Start-Process -FilePath $exe -ArgumentList "--dev-seed" `
    -NoNewWindow -PassThru `
    -RedirectStandardOutput $planeLog -RedirectStandardError $planeErr

# Wait for it to actually serve, rather than assuming a spawned process is a
# working one.
$ready = $false
for ($i = 0; $i -lt 40 -and -not $ready; $i++) {
    if ($plane.HasExited) {
        $why = (Get-Content $planeErr -Raw -ErrorAction SilentlyContinue)
        if (-not $why) { $why = (Get-Content $planeLog -Raw -ErrorAction SilentlyContinue) }
        throw "control plane exited immediately (code $($plane.ExitCode)). $why"
    }
    try {
        Invoke-RestMethod -Uri "http://127.0.0.1:8080/healthz" -TimeoutSec 2 | Out-Null
        $ready = $true
    } catch { Start-Sleep -Milliseconds 250 }
}
if (-not $ready) {
    Stop-Process -Id $plane.Id -Force -ErrorAction SilentlyContinue
    throw "control plane never answered /healthz"
}

Write-Host "control plane pid $($plane.Id) -> http://127.0.0.1:8080 (healthy)"

$tunnelLog = Join-Path $secretsDir "tunnel.log"
$tunnelErr = Join-Path $secretsDir "tunnel.err"
Reset-Log $tunnelLog
Reset-Log $tunnelErr

$tunnel = Start-Process -FilePath "cloudflared" `
    -ArgumentList "tunnel --url http://127.0.0.1:8080 --no-autoupdate" `
    -NoNewWindow -PassThru `
    -RedirectStandardOutput $tunnelLog -RedirectStandardError $tunnelErr

Write-Host "tunnel pid $($tunnel.Id), waiting for a hostname..."

$public = $null
for ($i = 0; $i -lt 120 -and -not $public; $i++) {
    Start-Sleep -Milliseconds 500
    foreach ($f in @($tunnelLog, $tunnelErr)) {
        if (Test-Path $f) {
            $m = Select-String -Path $f -Pattern "https://[a-z0-9-]+\.trycloudflare\.com" |
                 Select-Object -First 1
            if ($m) { $public = $m.Matches[0].Value; break }
        }
    }
}

if (-not $public) {
    Stop-Process -Id $plane.Id, $tunnel.Id -Force -ErrorAction SilentlyContinue
    throw "tunnel did not report a hostname; see $tunnelErr"
}

# Bind the repo to the seeded project. Without this every delivery is
# acknowledged and ignored - deliberately, since a repo is never inferred from
# a name, but it is also the most likely reason for "nothing happened".
# Retry: the token line is written at startup, but a redirected stream is
# buffered and may not have reached disk the first time we look.
# Read from the file the control plane writes it to, not from the log. It used
# to be logged, which put a working credential into every system the log
# reaches - see SEC-6 in docs/SECURITY-REVIEW.md.
$tokenFile = Join-Path $env:THETA_INSTANCE_ROOT "dev-identity-token"
$token = $null
for ($i = 0; $i -lt 20 -and -not $token; $i++) {
    if (Test-Path $tokenFile) {
        $token = (Get-Content $tokenFile -Raw).Trim()
    }
    if (-not $token) { Start-Sleep -Milliseconds 250 }
}

if ($token) {
    try {
        Invoke-RestMethod -Method Put `
            -Uri "http://127.0.0.1:8080/v1/projects/org_local%2Fscratch/github" `
            -Headers @{ authorization = "Bearer $token" } `
            -ContentType "application/json" `
            -Body (@{ repository = "FelixKramer/ThetaBase" } | ConvertTo-Json) | Out-Null
        Write-Host "bound FelixKramer/ThetaBase -> org_local/scratch"
    } catch {
        Write-Host "could not bind the repo: $($_.Exception.Message)"
    }
} else {
    Write-Host "no dev identity token found in the log; bind the repo by hand"
}

# Point the App at this run's hostname. A quick tunnel gets a new one every
# restart, and a webhook URL nobody keeps up with is a webhook that silently
# stops delivering - so update it from here rather than expecting a human to
# remember. Authenticated with the App's own key, which is the only credential
# permitted to change this.
if (Test-Path $pem) {
    $set = & python (Join-Path $repo "gh-app.py") set-webhook $public 2>&1
    Write-Host $set
} else {
    Write-Host "no App key yet - set the webhook URL by hand"
}

Write-Host ""
Write-Host "  public base URL   $public"
Write-Host "  webhook URL       $public/v1/github/webhook"
Write-Host ""
Write-Host "  Ctrl-C to stop both."

try {
    while (-not $plane.HasExited -and -not $tunnel.HasExited) { Start-Sleep -Seconds 1 }
} finally {
    Stop-Process -Id $plane.Id, $tunnel.Id -Force -ErrorAction SilentlyContinue
    Write-Host "stopped."
}
