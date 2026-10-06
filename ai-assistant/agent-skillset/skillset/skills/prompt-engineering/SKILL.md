---
name: prompt-engineering
description: 'Designs, creates, migrates, evaluates, and improves prompts, prompt sets, GitHub Copilot instructions, .prompt.md files, .agent.md custom agents, legacy .chatmode.md files, AGENTS.md files, and Agent Skills. Use for VS Code or Visual Studio prompt customization, meta-prompts, multi-agent workflows, model routing, mixture-of-experts-style review, frontmatter, discovery, tool scoping, and prompt quality testing.'
argument-hint: 'Describe the prompt artifact, target IDE, scope, users, and desired outcome'
---

# Prompt Engineering

Create prompt customizations that are discoverable, scoped, testable, maintainable, and compatible with their target host.

## Required Inputs

Establish these before writing. Infer from local evidence when safe, and ask only when the answer changes the artifact type or behavior.

- Outcome: the observable result the customization must produce.
- Target: VS Code, Visual Studio, GitHub Copilot CLI/cloud agent, or multiple hosts.
- Scope: personal cross-workspace use, one repository, or organization-wide use.
- Consumer: human-invoked workflow, automatically applied rule, or model-invoked capability.
- Constraints: tools, models, security boundaries, output format, and compatibility requirements.
- Evidence: existing customizations, repository conventions, failure examples, or evaluation feedback.

## Workflow

1. Inspect the nearest existing customization and the files or workflow it affects. Preserve local naming, model, tool, and frontmatter conventions unless they are invalid.
2. For platform-sensitive behavior, verify current official documentation. Use [Platform Reference](./references/platform-reference.md) to choose the primitive, location, and compatibility boundary.
3. Select one primary primitive. Split the solution only when separate lifecycle, scope, activation, or tool boundaries justify it.
4. Write a compact contract: objective, inputs, relevant context, ordered actions, constraints, output, failure behavior, and validation.
5. For multi-agent or multi-model work, use [Expert Routing](./references/expert-routing.md). Give each specialist a distinct question and require evidence that can be reconciled.
6. Create the smallest complete artifact using the appropriate file in [assets](./assets/). Remove unused template sections.
7. Validate filenames, paths, YAML frontmatter, referenced files, agent names, skill folder/name equality, tools, handoffs, and target-host support.
8. Run three behavioral scenarios: normal, underspecified, and edge or adversarial. Check whether the artifact asks only blocking questions, honors constraints, and produces the declared output.
9. Score the result with [Evaluation Rubric](./references/evaluation-rubric.md). Fix every zero and any issue that can change behavior.
10. Report files created or changed, compatibility limits, validation evidence, assumptions, and one or two representative invocations.

## Design Rules

- Put stable cross-task policy in instructions, not in every prompt.
- Put one manually invoked task in a prompt file.
- Put repeatable procedural knowledge and supporting resources in a skill.
- Put a persistent role, least-privilege tools, subagents, or handoffs in a custom agent.
- Use hooks only for deterministic enforcement that justifies running a command; never use them as decorative automation.
- Require explicit approval before hooks, execute or terminal access, destructive automation, networked lifecycle commands, or any persistent authority increase. Hooks are static, repository-local, non-destructive, non-networked, secret-free, and removable by default.
- Treat legacy chat modes as migration inputs. New VS Code artifacts use `.agent.md`.
- Prefer requirements and examples over adjectives such as "excellent", "robust", or "production ready".
- Do not ask for hidden chain-of-thought. Request concise decisions, evidence, checks, and assumptions.
- Never assume that selecting a model exposes or controls its internal mixture-of-experts architecture.

## Supporting References

- [Design Method](./references/design-method.md): requirement tracing, contracts, decomposition, and iteration.
- [Platform Reference](./references/platform-reference.md): primitive selection, locations, frontmatter, IDE compatibility, and current sources.
- [Expert Routing](./references/expert-routing.md): specialist delegation, model fallbacks, parallel review, and reconciliation.
- [Evaluation Rubric](./references/evaluation-rubric.md): static checks, behavioral scenarios, scoring, and release threshold.

## Templates

- [Instructions template](./assets/instructions.template.md)
- [Prompt file template](./assets/prompt.template.md)
- [Custom agent template](./assets/agent.template.md)
- [Agent Skill template](./assets/skill.template.md)

## Completion Standard

A customization is complete only when it is in a discoverable location, its metadata parses, no unresolved template placeholders remain, its references resolve, its privileges fit the task, representative scenarios meet the output contract, and platform-specific limitations are stated with official source evidence or clearly labeled assumptions.