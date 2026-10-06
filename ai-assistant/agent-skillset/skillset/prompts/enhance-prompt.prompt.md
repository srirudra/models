---
name: 'enhance-prompt'
description: 'Improve an existing prompt, instruction, custom agent, Agent Skill, or prompt set using observed failures, requirements, and current platform guidance.'
argument-hint: 'Artifact path or selected text; desired behavior; failures or feedback; target host'
agent: 'prompt-architect'
---

# Enhance Prompt

Improve the supplied artifact while preserving correct behavior and local conventions.

1. Read the full artifact and its directly referenced instructions or resources.
2. Extract the current contract and compare it with the requested behavior, failures, evaluation data, or user feedback.
3. Classify each material problem as discovery, primitive fit, missing context, ambiguity, conflict, unsupported capability, excess privilege, output contract, or validation gap.
4. Make the smallest structural or wording changes that address root causes. Move duplicated stable policy to an owning instruction or reference only when that reduces real drift.
5. Preserve user requirements, placeholders, examples, and public invocation names unless a change is necessary and reported.
6. Before widening tools, hooks, autonomy, lifecycle commands, destructive behavior, or persistent scope, obtain explicit approval naming the added capability and blast radius.
7. Validate frontmatter and references, then rerun the failing scenario plus one normal and one neighboring edge scenario.
8. Request an independent review for any shared artifact or change to tools, models, agents, handoffs, hooks, security boundaries, cross-host claims, migrations, or output contracts.

Return the files changed, behavioral differences, evidence for the improvements, compatibility notes, and unresolved assumptions.