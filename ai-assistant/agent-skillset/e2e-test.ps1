# End-to-end test of build\install.ps1, run via powershell -File to avoid
# the session tool's argument-passing quirk. Mirrors the MSBuild target invocation.
$ErrorActionPreference = 'Stop'
$script = Join-Path $PSScriptRoot 'build\install.ps1'
$tmp = Join-Path $env:TEMP ("agent-skillset-e2e-" + (Get-Random))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
$dest = Join-Path $tmp '.copilot'
$org = 'https://dev.azure.com/MyTestOrg'

function Run([string[]]$args) {
    & powershell -NoProfile -ExecutionPolicy Bypass -File $script @args
}

Write-Host "===== 1) PREVIEW ====="
Run @('-Target','repo','-RepoRoot',$tmp,'-List') | Select-Object -Last 1

Write-Host "`n===== 2) INSTALL with org ====="
Run @('-Target','repo','-RepoRoot',$tmp,'-AzureDevOpsOrg',$org,'-NonInteractive') | Select-Object -Last 6

Write-Host "`n===== 3) VERIFY LAYOUT ====="
Write-Host ("Agents:    {0}" -f (Get-ChildItem "$dest\agents" -File -ErrorAction SilentlyContinue).Count)
Write-Host ("Skills:    {0}" -f (Get-ChildItem "$dest\skills" -Directory -ErrorAction SilentlyContinue).Count)
Write-Host ("Instr:     {0}" -f (Get-ChildItem "$dest\instructions" -File -ErrorAction SilentlyContinue).Count)
Write-Host ("Prompts:   {0}" -f (Get-ChildItem "$dest\prompts" -File -ErrorAction SilentlyContinue).Count)
Write-Host ("Knowledge: {0} (expect 0)" -f (Get-ChildItem "$dest\knowledge" -ErrorAction SilentlyContinue | Measure-Object).Count)
Write-Host ("Manifest:  {0}" -f (Test-Path "$dest\.agent-skillset-manifest.json"))

Write-Host "`n===== 4) VERIFY ORG SUBSTITUTION ====="
$ab = Get-Content "$dest\skills\azure-boards\SKILL.md" -Raw
if ($ab -match 'MyTestOrg') { Write-Host "PASS: MyTestOrg present" } else { Write-Host "FAIL: MyTestOrg missing" }
if ($ab -match 'Redington-ADA') { Write-Host "FAIL: hardcoded org present" } else { Write-Host "PASS: no hardcoded org" }
if ($ab -match '\{\{AZURE_DEVOPS_ORG\}\}') { Write-Host "FAIL: placeholder left" } else { Write-Host "PASS: no placeholder" }

Write-Host "`n===== 5) IDEMPOTENCY (re-run, expect all skipped) ====="
Run @('-Target','repo','-RepoRoot',$tmp,'-AzureDevOpsOrg',$org,'-NonInteractive') | Select-String "copied|skipped"

Write-Host "`n===== 6) UNINSTALL ====="
Run @('-Target','repo','-RepoRoot',$tmp,'-Uninstall') | Select-Object -Last 1
Write-Host ("Files left after uninstall: {0} (expect 0)" -f (Get-ChildItem $dest -Recurse -File -ErrorAction SilentlyContinue | Measure-Object).Count)

Remove-Item $tmp -Recurse -Force
Write-Host "`nCleaned up."
