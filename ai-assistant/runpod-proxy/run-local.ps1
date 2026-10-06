<#
.SYNOPSIS
    Runs runpod-proxy directly on the Windows host instead of in Docker.

.DESCRIPTION
    Docker containers on this machine cannot reach the RunPod API because the
    corporate TLS-inspecting proxy does not tunnel Hyper-V/WSL NAT traffic.
    Running on the host works, but the host proxy re-signs TLS with a root CA
    that is not in certifi's bundle, so this script builds a combined
    certifi + Windows-root-store bundle and points SSL_CERT_FILE at it.

    Loads the env file into the process (the app reads plain environment
    variables), then starts uvicorn from the local virtualenv.

.EXAMPLE
    .\run-local.ps1
    .\run-local.ps1 -Port 9090 -EnvFile .env.demo
#>
[CmdletBinding()]
param(
    [string]$EnvFile = '.env',
    [int]$Port = 0,
    [string]$BindHost = '0.0.0.0',
    [switch]$Reload,
    # Rebuild the CA bundle even if it already exists (e.g. after a root CA rotation).
    [switch]$RefreshCaBundle
)

$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath $PSScriptRoot

$python = Join-Path $PSScriptRoot '.venv\Scripts\python.exe'
if (-not (Test-Path -LiteralPath $python)) {
    throw "Virtualenv not found at $python. Create it with: py -3.12 -m venv .venv; .\.venv\Scripts\python.exe -m pip install -r requirements.txt"
}

$envPath = if ([System.IO.Path]::IsPathRooted($EnvFile)) { $EnvFile } else { Join-Path $PSScriptRoot $EnvFile }
if (-not (Test-Path -LiteralPath $envPath)) {
    throw "Env file not found: $envPath"
}

$loaded = 0
foreach ($line in Get-Content -LiteralPath $envPath) {
    $trimmed = $line.Trim()
    if (-not $trimmed -or $trimmed.StartsWith('#')) { continue }
    if ($trimmed -notmatch '^([A-Za-z_][A-Za-z0-9_]*)=(.*)$') { continue }
    $name = $Matches[1]
    $value = $Matches[2].Trim()
    # Match docker compose env_file semantics: strip one layer of matching quotes.
    if ($value.Length -ge 2 -and (($value.StartsWith("'") -and $value.EndsWith("'")) -or ($value.StartsWith('"') -and $value.EndsWith('"')))) {
        $value = $value.Substring(1, $value.Length - 2)
    }
    Set-Item -Path "Env:$name" -Value $value
    $loaded++
}
Write-Host "Loaded $loaded variables from $EnvFile (values not echoed)."

$caBundle = Join-Path $env:LOCALAPPDATA 'runpod-proxy\ca-bundle.pem'
if ($RefreshCaBundle -or -not (Test-Path -LiteralPath $caBundle)) {
    Write-Host 'Building CA bundle from certifi + Windows root store...'
    New-Item -ItemType Directory -Force -Path (Split-Path -Parent $caBundle) | Out-Null
    $certifiPath = & $python -c 'import certifi;print(certifi.where())'
    $builder = [System.Text.StringBuilder]::new()
    [void]$builder.AppendLine((Get-Content -LiteralPath $certifiPath -Raw))
    foreach ($cert in Get-ChildItem -Path Cert:\LocalMachine\Root, Cert:\CurrentUser\Root | Sort-Object Thumbprint -Unique) {
        [void]$builder.AppendLine("# $($cert.Subject)")
        [void]$builder.AppendLine('-----BEGIN CERTIFICATE-----')
        [void]$builder.AppendLine([Convert]::ToBase64String($cert.RawData, 'InsertLineBreaks'))
        [void]$builder.AppendLine('-----END CERTIFICATE-----')
    }
    Set-Content -LiteralPath $caBundle -Value $builder.ToString() -Encoding ascii
}
$env:SSL_CERT_FILE = $caBundle
Write-Host "SSL_CERT_FILE = $caBundle"

if ($Port -le 0) {
    $Port = if ($env:PORT) { [int]$env:PORT } else { 8080 }
}

$inUse = Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction SilentlyContinue
if ($inUse) {
    throw "Port $Port is already in use (PID $($inUse.OwningProcess -join ', ')). Stop that process, or 'docker compose stop' if the container is running."
}

$uvicornArgs = @('-m', 'uvicorn', 'proxy.main:app', '--host', $BindHost, '--port', $Port)
if ($Reload) { $uvicornArgs += '--reload' }

Write-Host "Starting runpod-proxy on http://${BindHost}:$Port (Ctrl+C to stop)."
& $python @uvicornArgs
