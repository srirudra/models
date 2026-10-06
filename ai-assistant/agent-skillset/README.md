# AgentSkillset

A portable, installable set of **GitHub Copilot** customizations — custom agents, Agent Skills,
instruction files, and slash prompts — packaged as a NuGet package so you can share and reproduce
your Copilot setup across machines and teams.

This package bundles a complete, working multi-agent delivery workflow (the "Principal Engineer"
team), a prompt-authoring toolkit, and a library of domain skills for .NET/Azure engineering.

## What's inside

| Category | Count | Details |
|----------|-------|---------|
| **Agents** | 8 | `principal-engineer` (visible, accountable delivery owner), `software-engineer`, `cloud-engineer`, `architect`, `security-engineer`, `qa-engineer` (hidden workers), `prompt-architect` (visible), `prompt-reviewer` (hidden) |
| **Skills** | 15 | `pbi-delivery`, `pbi-planning`, `prompt-engineering`, `azure-boards`, `adr-writing`, `architecture-review`, `cloud-change-review`, `codebase-onboarding`, `dependency-upgrade`, `dotnet-build-test`, `incident-debugging`, `release-management`, `runbook-writing`, `security-review`, `threat-modeling` |
| **Instructions** | 7 | `csharp-dotnet`, `devops-release`, `dotnet-testing`, `engineering-docs`, `prompt-authoring`, `repo-investigation`, `security` |
| **Prompts** | 26 | PBI delivery, prompt authoring, and domain slash prompts (ADR, code review, security review, threat model, runbook, release, incident debug, and more) |
| **Workflow docs** | 2 | `TEAM_WORKFLOW.md` (authoritative workflow), `WORKFLOW_CHANGELOG.md` |
| **Plugin** | 1 | `ai-team-orchestration` (third-party, MIT, Denis Evdokimov) |

The agents form an acyclic consultation graph (Principal → Software/Cloud/Architect/Security/QA,
with cross-consultation among the workers). See `skillset/workflow/TEAM_WORKFLOW.md` for the full
design.

## Install

### Option A — NuGet (recommended)

```powershell
# In any .NET project (or a dedicated "tooling" project):
dotnet add package AgentSkillset
```

On the first build, the package's `build/AgentSkillset.targets` runs the installer once per
machine (marker file) and installs to your **user-level** `~/.copilot` by default.

Customize via MSBuild properties (set in your project before referencing the package):

```xml
<PropertyGroup>
  <AgentSkillsetTarget>repo</AgentSkillsetTarget>              <!-- user (default) | repo -->
  <AgentSkillsetRepoRoot>$(MSBuildProjectDirectory)</AgentSkillsetRepoRoot>
  <AgentSkillsetAzureDevOpsOrg>https://dev.azure.com/MyOrg</AgentSkillsetAzureDevOpsOrg>
  <AgentSkillsetIncludeKnowledge>true</AgentSkillsetIncludeKnowledge>
  <AgentSkillsetSetSubagentSetting>true</AgentSkillsetSetSubagentSetting>
  <AgentSkillsetSkipInstall>true</AgentSkillsetSkipInstall>    <!-- opt out of build-time install -->
</PropertyGroup>
```

### Option B — Manual (no .NET project needed)

```powershell
# Preview what would be installed:
powershell -ExecutionPolicy Bypass -File .\build\install.ps1 -List

# Install to your user-level ~/.copilot (default):
powershell -ExecutionPolicy Bypass -File .\build\install.ps1

# Install into a repository's .copilot folder for team sharing:
powershell -ExecutionPolicy Bypass -File .\build\install.ps1 -Target repo -RepoRoot C:\src\MyRepo -AzureDevOpsOrg https://dev.azure.com/MyOrg

# Uninstall (removes exactly the files this package installed):
powershell -ExecutionPolicy Bypass -File .\build\install.ps1 -Uninstall
```

### Installer options

| Parameter | Default | Description |
|-----------|---------|-------------|
| `-Target` | `user` | `user` → `~/.copilot` (+ VS Code prompt sync); `repo` → `<repoRoot>\.copilot` |
| `-RepoRoot` | cwd | Repository root when `-Target repo` |
| `-Force` | off | Overwrite files that already exist |
| `-List` | off | Preview the install without copying |
| `-AzureDevOpsOrg` | — | Substituted for `{{AZURE_DEVOPS_ORG}}` in the azure-boards skill |
| `-IncludeKnowledge` | off | Also install the personal `knowledge/` files |
| `-ExcludeAiTeamOrchestration` | off | Skip the third-party plugin |
| `-SkipVsCodeSync` | off | Skip syncing prompts to the VS Code user profile |
| `-SetSubagentSetting` | off | Add `chat.subagents.allowInvocationsFromSubagents: true` to VS Code settings |
| `-Uninstall` | off | Remove previously installed files (uses the manifest) |
| `-NonInteractive` | off | Never prompt (used by the MSBuild target) |

## Install targets

- **User-level** (`~/.copilot`): personal, machine-wide. Also syncs prompts to the VS Code user
  profile (`%APPDATA%\Code\User\prompts`) so they appear as slash commands.
- **Repository-level** (`.copilot`): committed to source control so a whole team gets the same
  agents/skills/prompts.

The install is **non-destructive** by default — existing files are left untouched. A manifest
(`.agent-skillset-manifest.json`) is written to the target root so `-Uninstall` removes exactly
what this package installed.

## Portability notes

- **azure-boards skill** — the Azure DevOps organization is parameterized as `{{AZURE_DEVOPS_ORG}}`.
  Set it with `-AzureDevOpsOrg` at install time, or edit `skills/azure-boards/SKILL.md` directly.
- **knowledge/** — contains personal repository paths and is **excluded by default**. Opt in with
  `-IncludeKnowledge` if you want it.
- **snyk_rules.instructions.md** — Snyk-managed (auto-regenerated) and **not included** in this
  package. Snyk will regenerate it on your machine.
- **Model pins** — agents and prompts pin specific models (e.g. GPT-5.5, Claude Opus 4.8). These
  are intentional (model fallback arrays) and are preserved. Adjust them if your environment
  doesn't have those models.

## Required VS Code setting

The multi-agent consultation workflow requires subagents to be able to invoke other subagents.
Add to your VS Code `settings.json` (or run the installer with `-SetSubagentSetting`):

```json
{
  "chat.subagents.allowInvocationsFromSubagents": true
}
```

## After installing

1. Restart VS Code (or reload the Copilot extension) to pick up the new agents/skills.
2. Invoke the entry point: `/principal-engineer` (or ask it to deliver a PBI).
3. For team installs, commit the `.copilot` folder.

## Building the package

```powershell
nuget pack AgentSkillset.nuspec -OutputDirectory nupkg
# or
dotnet pack AgentSkillset.nuspec -o nupkg
```

## License

MIT. The bundled `ai-team-orchestration` plugin is third-party (MIT, Denis Evdokimov) and is
distributed under its own license.
