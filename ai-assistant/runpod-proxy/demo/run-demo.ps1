# One-command stakeholder demo for the RunPod serverless warm proxy.
#
# Starts the mock serverless upstream (simulated cold start) + the proxy
# container (using .env.demo, short intervals), then walks through the
# full lifecycle: COLD -> warm -> streaming chat -> idle give-up -> COLD.
#
#   .\demo\run-demo.ps1          # run demo, leave stack running
#   .\demo\run-demo.ps1 -Cleanup # run demo, then stop everything

param([switch]$Cleanup)

$ErrorActionPreference = "Stop"

# JSON bodies go through temp files (curl -d @file): Windows PowerShell 5.1
# mangles double-quoted -d arguments passed to native exes.
function Get-TempBody {
    param([Parameter(Mandatory)][string]$Json)
    $f = Join-Path $env:TEMP ("rp-demo-" + [guid]::NewGuid().ToString('N') + ".json")
    Set-Content -Path $f -Value $Json -NoNewline -Encoding ascii
    return $f
}
$root = Split-Path $PSScriptRoot -Parent

Write-Host "Starting mock serverless upstream on :9999 ..." -ForegroundColor Cyan
$mock = Start-Process -FilePath python -ArgumentList "$PSScriptRoot\mock_upstream.py" `
    -WorkingDirectory $root -PassThru -WindowStyle Hidden
Start-Sleep 1

$env:ENV_FILE = ".env.demo"
Set-Location $root
Write-Host "Starting proxy container (docker compose, env .env.demo) ..." -ForegroundColor Cyan
# stop any stale instance so every demo run starts from a clean COLD state
docker compose down --remove-orphans 2>$null | Out-Null
docker compose up -d --build | Out-Null
Start-Sleep 2

try {
    Write-Host "`n=== 1. Initial state (COLD - no worker running, nothing billed):" -ForegroundColor Cyan
    curl.exe -s http://localhost:8080/_status
    Write-Host "`n=== 2. First request warms the endpoint (mock simulates a 3s cold start):" -ForegroundColor Cyan
    curl.exe -s -X POST http://localhost:8080/_warm -w "`n   [http %{http_code} in %{time_total}s]"
    Write-Host "`n=== 3. State after warmup (WARM - keepalives run every 2s while active):" -ForegroundColor Cyan
    curl.exe -s http://localhost:8080/_status
    Write-Host "`n=== 4. Model list through the proxy (path mapped onto the endpoint):" -ForegroundColor Cyan
    curl.exe -s http://localhost:8080/v1/models
    Write-Host "`n=== 5. Streaming chat completion - SSE passes through the proxy:" -ForegroundColor Cyan
    $body = Get-TempBody '{"model":"demo-qwen-7b","stream":true,"messages":[{"role":"user","content":"hi"}]}'
    curl.exe -sN -X POST http://localhost:8080/v1/chat/completions -H "Content-Type: application/json" -d "@$body"
    Remove-Item $body -ErrorAction SilentlyContinue
    Write-Host "`n=== 6. Non-streaming chat - note the mock echoes the auth it received:" -ForegroundColor Cyan
    $body = Get-TempBody '{"model":"demo-qwen-7b","stream":false,"messages":[{"role":"user","content":"hi"}]}'
    curl.exe -s -X POST http://localhost:8080/v1/chat/completions -H "Content-Type: application/json" -d "@$body"
    Remove-Item $body -ErrorAction SilentlyContinue
    Write-Host "`n=== 7. Waiting 12s ... no real traffic, so after IDLE_GIVEUP_S=8s the proxy" -ForegroundColor Cyan
    Write-Host "    stops keepalives and lets RunPod recycle the worker (billing stops):" -ForegroundColor Cyan
    Start-Sleep 12
    Write-Host "`n=== 8. State after idle (back to COLD):" -ForegroundColor Cyan
    curl.exe -s http://localhost:8080/_status
    Write-Host "`n=== Mock upstream request log (every line shows the injected auth=demo-rp-key):" -ForegroundColor Cyan
    Get-Content "$PSScriptRoot\mock-upstream.log"
    Write-Host "`nDone. Container logs:  docker compose logs runpod-proxy" -ForegroundColor Green
}
finally {
    if ($Cleanup) {
        Write-Host "Cleanup: stopping proxy container and mock upstream ..." -ForegroundColor Yellow
        docker compose down | Out-Null
        Stop-Process -Id $mock.Id -Force -ErrorAction SilentlyContinue
    }
}
