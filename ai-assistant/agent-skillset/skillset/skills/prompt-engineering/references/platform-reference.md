# Platform Reference

Verify platform-sensitive details against current official documentation before changing an established setup. This reference reflects documentation available on 2026-07-23.

## Primitive Selection

| Need | Primary primitive | Why |
|---|---|---|
| Stable repository-wide conventions | `.github/copilot-instructions.md` or `AGENTS.md` | Automatically supplied to chat |
| Rules for a file type, folder, or semantically matched task | `*.instructions.md` | Conditional activation through `applyTo` or description |
| One reusable human-invoked task | `*.prompt.md` | Focused slash command with optional inputs, agent, model, and tools |
| Reusable multi-step capability with references, assets, or scripts | `SKILL.md` | Progressive loading and portability across skills-compatible agents |
| Persistent persona, restricted tools, subagents, or handoffs | `*.agent.md` | Role and capability boundary |
| Deterministic lifecycle enforcement | Hook JSON or agent `hooks` | Runs a command at a defined event |

Do not create several primitives to repeat the same policy. Reference the owning artifact instead.

## Default Locations

| Artifact | Repository | Personal |
|---|---|---|
| Always-on instructions | `.github/copilot-instructions.md`, `AGENTS.md` | Host-dependent |
| File instructions | `.github/instructions/*.instructions.md` | VS Code user profile or `~/.copilot/instructions/` |
| Prompt files | `.github/prompts/*.prompt.md` | VS Code user profile |
| Custom agents | `.github/agents/*.agent.md` | VS Code user profile or `~/.copilot/agents/` |
| Agent Skills | `.github/skills/<name>/SKILL.md` | `~/.copilot/skills/<name>/SKILL.md` |

User-profile customizations are personal. Repository customizations are versioned and shared with the team.

## Host Compatibility

| Capability | VS Code | Visual Studio |
|---|---|---|
| `.github/copilot-instructions.md` | Supported | Supported when custom instructions are enabled |
| `.github/instructions/*.instructions.md` | Supported | Supported when custom instructions are enabled |
| `.github/prompts/*.prompt.md` | Supported as slash prompts | Supported as repository prompt files and slash prompts in current releases |
| `.agent.md` custom agents and handoffs | Supported | Do not use as a portable entrypoint unless the installed Visual Studio release explicitly documents support |
| Agent Skills | Supported in current Copilot agent experiences | Do not use as a portable entrypoint unless the installed Visual Studio release explicitly documents support |
| `.chatmode.md` | Legacy; rename and migrate to `.agent.md` | Not a portable customization target |

For a toolkit that must work in both IDEs, place shared behavior in repository instructions and prompt files. Add VS Code agents or skills as optional orchestration layers.

Record the official sources and product version or documentation date used for compatibility decisions. If current sources are unavailable, label the decision as an assumption and avoid destructive migration.

## Frontmatter Essentials

### Instructions

- `description`: keyword-rich semantic discovery text.
- `applyTo`: optional glob or glob list for automatic matching.
- Avoid `applyTo: '**'` unless every request truly needs the content.

### Prompt Files

- Recommended: `name`, `description`, `argument-hint`, and `agent`.
- Optional: `model` and least-privilege `tools`.
- Prompt-level tools override tools inherited from the referenced agent.

### Custom Agents

- Make `name` equal the kebab-case filename stem for deterministic local references.
- If `agents` is declared, include `agent` in `tools`.
- `user-invocable: false` hides an agent from the picker while allowing subagent use.
- `disable-model-invocation: true` keeps an agent user-invocable but blocks subagent invocation.
- Avoid circular handoffs.

### Agent Skills

- `name` is 1-64 lowercase letters, numbers, or hyphens and exactly matches the parent folder.
- `description` states what the skill does and when it should load.
- Keep `SKILL.md` concise and reference focused resources one level deep.

## Chat Mode Migration

1. Preserve the body instructions and behavior.
2. Rename `<name>.chatmode.md` to `<name>.agent.md`.
3. Normalize current frontmatter fields and least-privilege tool aliases.
4. Make the `name` and filename stem resolve consistently.
5. Replace references to chat modes with custom agents.
6. Validate handoffs, subagent names, tools, and model fallbacks in the target VS Code version.

## Official Sources

- [VS Code custom instructions](https://code.visualstudio.com/docs/agent-customization/custom-instructions)
- [VS Code prompt files](https://code.visualstudio.com/docs/agent-customization/prompt-files)
- [VS Code custom agents](https://code.visualstudio.com/docs/agent-customization/custom-agents)
- [VS Code Agent Skills](https://code.visualstudio.com/docs/agent-customization/agent-skills)
- [Visual Studio chat customization](https://learn.microsoft.com/visualstudio/ide/copilot-chat-context)
- [GitHub Copilot prompt engineering](https://docs.github.com/copilot/concepts/prompting/prompt-engineering)
- [Agent Skills specification](https://agentskills.io/specification)
- [Awesome Copilot examples](https://github.com/github/awesome-copilot)