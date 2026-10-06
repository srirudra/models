<#
.SYNOPSIS
    Installs the AgentSkillset (GitHub Copilot agents, skills, instructions, and prompts)
    to a user-level or repository-level Copilot discovery location.

.DESCRIPTION
    Copies the packaged skillset into the target location:
      - Target 'user' (default): %USERPROFILE%\.copilot\  (or ~/.copilot on Linux/macOS)
        plus a prompt sync to the VS Code user profile (%APPDATA%\Code\User\prompts).
      - Target 'repo': <repoRoot>\.copilot\  (for team sharing via source control)

    The install is non-destructive by default: existing files are left untouched.
    Use -Force to overwrite. A manifest (.agent-skillset-manifest.json) is written to
    the target root so -Uninstall can remove exactly the files this package installed.

.PARAMETER Target
    'user' (default) or 'repo'.

.PARAMETER RepoRoot
    Repository root when -Target repo. Defaults to the current directory.

.PARAMETER Force
    Overwrite files that already exist in the target.

.PARAMETER List
    Show what would be installed (and its status) without copying anything.

.PARAMETER AzureDevOpsOrg
    Azure DevOps organization URL (e.g. https://dev.azure.com/MyOrg) substituted for
    the {{AZURE_DEVOPS_ORG}} placeholder in the azure-boards skill.

.PARAMETER IncludeKnowledge
    Also install the personal knowledge/ files (skipped by default).

.PARAMETER ExcludeAiTeamOrchestration
    Skip the third-party ai-team-orchestration plugin (MIT, Denis Evdokimov).

.PARAMETER SkipVsCodeSync
    Skip syncing prompts to the VS Code user profile (user target only).

.PARAMETER SetSubagentSetting
    Add "chat.subagents.allowInvocationsFromSubagents": true to VS Code settings.json
    (user target only). Required for the multi-agent consultation workflow.

.PARAMETER Uninstall
    Remove the files previously installed by this package (uses the manifest).

.PARAMETER NonInteractive
    Never prompt. Used by the MSBuild build target.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\build\install.ps1
    Install to the user-level ~/.copilot (non-destructive).

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\build\install.ps1 -Target repo -RepoRoot C:\src\MyRepo -AzureDevOpsOrg https://dev.azure.com/MyOrg
    Install into a repository's .copilot folder with a custom Azure DevOps org.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\build\install.ps1 -List
    Preview the install without copying.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\build\install.ps1 -Uninstall
    Remove everything this package installed.
#>
[CmdletBinding()]
param(
    [ValidateSet('user', 'repo')]
    [string]$Target = 'user',

    [string]$RepoRoot = '',

    [switch]$Force,
    [switch]$List,
    [string]$AzureDevOpsOrg = '',
    [switch]$IncludeKnowledge,
    [switch]$ExcludeAiTeamOrchestration,
    [switch]$SkipVsCodeSync,
    [switch]$SetSubagentSetting,
    [switch]$Uninstall,
    [switch]$NonInteractive
)

$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------------------
# Resolve package content root (works from repo, nupkg build/, or nupkg tools/)
# ---------------------------------------------------------------------------
$PackageRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$ContentRoot = Join-Path $PackageRoot 'skillset'
if (-not (Test-Path $ContentRoot)) {
    throw "Content root not found: $ContentRoot"
}
$VersionFile = Join-Path $PackageRoot 'VERSION'
$Version = if (Test-Path $VersionFile) { (Get-Content $VersionFile -Raw).Trim() } else { '0.0.0' }

# ---------------------------------------------------------------------------
# Resolve target roots (cross-platform)
# ---------------------------------------------------------------------------
$IsUnix = ($IsLinux -or $IsMacOS) -and -not $IsWindows
if ($IsUnix) {
    $UserCopilotRoot = Join-Path $HOME '.copilot'
    $VsCodePromptsDir = Join-Path $HOME '.config/Code/User/prompts'
    $VsCodeSettingsFile = Join-Path $HOME '.config/Code/User/settings.json'
} else {
    $UserCopilotRoot = Join-Path $env:USERPROFILE '.copilot'
    $VsCodePromptsDir = Join-Path $env:APPDATA 'Code\User\prompts'
    $VsCodeSettingsFile = Join-Path $env:APPDATA 'Code\User\settings.json'
}

if ($Target -eq 'user') {
    $TargetRoot = $UserCopilotRoot
} else {
    $repoBase = if ($RepoRoot) { $RepoRoot } else { (Get-Location).Path }
    $TargetRoot = Join-Path (Resolve-Path $repoBase).Path '.copilot'
}

$ManifestFile = Join-Path $TargetRoot '.agent-skillset-manifest.json'

# ---------------------------------------------------------------------------
# Uninstall
# ---------------------------------------------------------------------------
if ($Uninstall) {
    if (-not (Test-Path $ManifestFile)) {
        Write-Host "No manifest found at $ManifestFile - nothing to uninstall." -ForegroundColor Yellow
        return
    }
    $manifest = Get-Content $ManifestFile -Raw | ConvertFrom-Json
    $removed = 0
    foreach ($rel in @($manifest.files)) {
        $p = Join-Path $TargetRoot $rel
        if (Test-Path $p) { Remove-Item $p -Force; $removed++ }
    }
    foreach ($rel in @($manifest.vsCodeFiles)) {
        $p = Join-Path $VsCodePromptsDir $rel
        if (Test-Path $p) { Remove-Item $p -Force; $removed++ }
    }
    Remove-Item $ManifestFile -Force
    Write-Host "Uninstalled AgentSkillset $Version from $TargetRoot ($removed file(s) removed)." -ForegroundColor Green
    return
}

# ---------------------------------------------------------------------------
# Build the install file list: [src, destRel]
# ---------------------------------------------------------------------------
$files = New-Object System.Collections.Generic.List[object]

function Add-File($src, $destRel) {
    $files.Add([pscustomobject]@{ Src = $src; Dest = $destRel })
}

# Core agents
Get-ChildItem (Join-Path $ContentRoot 'agents') -Filter '*.agent.md' -File | ForEach-Object {
    Add-File $_.FullName (Join-Path 'agents' $_.Name)
}

# Skills (full trees: SKILL.md + references/ + assets/)
Get-ChildItem (Join-Path $ContentRoot 'skills') -Recurse -File | ForEach-Object {
    $rel = $_.FullName.Substring((Join-Path $ContentRoot 'skills').Length + 1)
    Add-File $_.FullName (Join-Path 'skills' $rel)
}

# Instructions
Get-ChildItem (Join-Path $ContentRoot 'instructions') -Filter '*.instructions.md' -File | ForEach-Object {
    Add-File $_.FullName (Join-Path 'instructions' $_.Name)
}

# Prompts
Get-ChildItem (Join-Path $ContentRoot 'prompts') -Filter '*.prompt.md' -File | ForEach-Object {
    Add-File $_.FullName (Join-Path 'prompts' $_.Name)
}

# Workflow docs
foreach ($doc in @('TEAM_WORKFLOW.md', 'WORKFLOW_CHANGELOG.md')) {
    $src = Join-Path (Join-Path $ContentRoot 'workflow') $doc
    if (Test-Path $src) { Add-File $src $doc }
}

# Personal knowledge (opt-in)
if ($IncludeKnowledge) {
    $kRoot = Join-Path $ContentRoot 'knowledge'
    if (Test-Path $kRoot) {
        Get-ChildItem $kRoot -Recurse -File | ForEach-Object {
            $rel = $_.FullName.Substring($kRoot.Length + 1)
            Add-File $_.FullName (Join-Path 'knowledge' $rel)
        }
    }
}

# Third-party ai-team-orchestration plugin (MIT)
if (-not $ExcludeAiTeamOrchestration) {
    $atRoot = Join-Path $ContentRoot 'ai-team-orchestration'
    if (Test-Path $atRoot) {
        Get-ChildItem (Join-Path $atRoot 'agents') -Filter '*.md' -File | ForEach-Object {
            Add-File $_.FullName (Join-Path 'agents' $_.Name)
        }
        $atSkill = Join-Path $atRoot 'skills/ai-team-orchestration'
        if (Test-Path $atSkill) {
            Get-ChildItem $atSkill -Recurse -File | ForEach-Object {
                $rel = $_.FullName.Substring($atSkill.Length + 1)
                Add-File $_.FullName (Join-Path 'skills/ai-team-orchestration' $rel)
            }
        }
    }
}

# VS Code prompt sync (user target only)
$vsCodeFiles = New-Object System.Collections.Generic.List[object]
if ($Target -eq 'user' -and -not $SkipVsCodeSync) {
    Get-ChildItem (Join-Path $ContentRoot 'prompts') -Filter '*.prompt.md' -File | ForEach-Object {
        $vsCodeFiles.Add([pscustomobject]@{ Src = $_.FullName; Dest = $_.Name })
    }
}

# ---------------------------------------------------------------------------
# List mode
# ---------------------------------------------------------------------------
if ($List) {
    Write-Host "AgentSkillset $Version - install preview (target: $Target -> $TargetRoot)"
    Write-Host ('-' * 100)
    foreach ($f in $files) {
        $dest = Join-Path $TargetRoot $f.Dest
        $status = if (Test-Path $dest) { 'exists' } else { 'new' }
        $mark = if ($status -eq 'exists' -and -not $Force) { 'skip' } else { 'copy' }
        Write-Host ("{0,-6} {1} -> {2}" -f $mark, $f.Dest, $status)
    }
    foreach ($f in $vsCodeFiles) {
        $dest = Join-Path $VsCodePromptsDir $f.Dest
        $status = if (Test-Path $dest) { 'exists' } else { 'new' }
        $mark = if ($status -eq 'exists' -and -not $Force) { 'skip' } else { 'copy' }
        Write-Host ("{0,-6} [vscode] {1} -> {2}" -f $mark, $f.Dest, $status)
    }
    Write-Host ('-' * 100)
    Write-Host ("Total: {0} file(s) in target, {1} VS Code sync file(s)" -f $files.Count, $vsCodeFiles.Count)
    return
}

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------
$copied = 0; $skipped = 0
$installed = New-Object System.Collections.Generic.List[string]
$vsCodeInstalled = New-Object System.Collections.Generic.List[string]

function Copy-One($src, $dest, $destRel, $list) {
    $normalized = $destRel.Replace('\', '/')
    if (Test-Path $dest -and -not $Force) {
        $script:skipped++
        return
    }
    $dir = Split-Path $dest -Parent
    if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }

    # Org substitution for the azure-boards skill
    if ($AzureDevOpsOrg -and $normalized -like 'skills/azure-boards/*') {
        $content = Get-Content $src -Raw
        $content = $content.Replace('{{AZURE_DEVOPS_ORG}}', $AzureDevOpsOrg)
        Set-Content -Path $dest -Value $content -NoNewline
    } else {
        Copy-Item -Path $src -Destination $dest -Force
    }
    $list.Add($destRel)
    $script:copied++
}

foreach ($f in $files) {
    $dest = Join-Path $TargetRoot $f.Dest
    Copy-One $f.Src $dest $f.Dest $installed
}
foreach ($f in $vsCodeFiles) {
    $dest = Join-Path $VsCodePromptsDir $f.Dest
    Copy-One $f.Src $dest $f.Dest $vsCodeInstalled
}

# ---------------------------------------------------------------------------
# VS Code subagent setting (user target only)
# ---------------------------------------------------------------------------
if ($SetSubagentSetting -and $Target -eq 'user') {
    $key = '"chat.subagents.allowInvocationsFromSubagents"'
    if (Test-Path $VsCodeSettingsFile) {
        $settings = Get-Content $VsCodeSettingsFile -Raw
        if ($settings -notmatch [regex]::Escape($key)) {
            $settings = $settings -replace '(\{)', "`$1`n    `"$key`: true,"
            Set-Content -Path $VsCodeSettingsFile -Value $settings -NoNewline
            Write-Host "Added $key to $VsCodeSettingsFile" -ForegroundColor Cyan
        } else {
            Write-Host "Subagent setting already present in VS Code settings." -ForegroundColor DarkGray
        }
    } else {
        Write-Host "VS Code settings.json not found at $VsCodeSettingsFile - skipping setting." -ForegroundColor Yellow
    }
}

# ---------------------------------------------------------------------------
# Write manifest (merge with existing so re-installs keep the full file list)
# ---------------------------------------------------------------------------
$existingManifest = $null
if (Test-Path $ManifestFile) {
    $existingManifest = Get-Content $ManifestFile -Raw | ConvertFrom-Json
}
$allFiles = @($installed)
if ($existingManifest) { $allFiles += @($existingManifest.files) }
$allVsCode = @($vsCodeInstalled)
if ($existingManifest) { $allVsCode += @($existingManifest.vsCodeFiles) }

$manifestObj = [pscustomobject]@{
    package       = 'AgentSkillset'
    version       = $Version
    installedAt   = (Get-Date).ToString('o')
    target        = $Target
    files         = ($allFiles | Sort-Object -Unique)
    vsCodeFiles   = ($allVsCode | Sort-Object -Unique)
}
$manifestDir = Split-Path $ManifestFile -Parent
if (-not (Test-Path $manifestDir)) { New-Item -ItemType Directory -Path $manifestDir -Force | Out-Null }
$manifestObj | ConvertTo-Json -Depth 5 | Set-Content -Path $ManifestFile -Encoding UTF8

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
Write-Host ''
Write-Host "AgentSkillset $Version installed to: $TargetRoot" -ForegroundColor Green
Write-Host ("  {0} file(s) copied, {1} skipped (already present)" -f $copied, $skipped)
if ($vsCodeFiles.Count -gt 0) {
    Write-Host ("  VS Code prompt sync: {0} file(s) -> {1}" -f $vsCodeFiles.Count, $VsCodePromptsDir)
}
if (-not $IncludeKnowledge) {
    Write-Host '  knowledge/ skipped (personal content; use -IncludeKnowledge to install it)' -ForegroundColor DarkGray
}
if ($AzureDevOpsOrg) {
    Write-Host "  azure-boards org set to: $AzureDevOpsOrg" -ForegroundColor DarkGray
} else {
    Write-Host '  azure-boards skill still contains the {{AZURE_DEVOPS_ORG}} placeholder - set it with -AzureDevOpsOrg' -ForegroundColor Yellow
}
Write-Host ''
Write-Host 'Next steps:'
Write-Host '  1. Restart VS Code (or run /agents reload) to pick up new agents/skills.'
if ($Target -eq 'user') {
    Write-Host '  2. For the multi-agent workflow, ensure "chat.subagents.allowInvocationsFromSubagents": true'
    Write-Host '     in VS Code settings (or re-run with -SetSubagentSetting).'
} else {
    Write-Host '  2. Commit the .copilot folder to share the skillset with your team.'
}
Write-Host '  3. Uninstall anytime with: -Uninstall'
