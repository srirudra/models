---
name: codebase-onboarding
description: 'Create concise onboarding briefs for unfamiliar .NET codebase areas. Use when mapping services, entry points, dependencies, tests, configuration, deployment, ownership, and risks.'
user-invocable: false
---

# Codebase Onboarding

Use this skill to quickly understand a codebase area without over-reading.

## Procedure

1. Start from the requested feature, service, project, file, symbol, or failing behavior.
2. Identify entry points, owning projects, important interfaces/classes, configuration, data stores, external services, and deployment path.
3. Read nearby tests and build/release files only where they clarify behavior or validation.
4. Note operational concerns: logging, monitoring, queues, retries, config transforms, secrets, and rollback.
5. Call out unknowns and the cheapest reads or commands that would disconfirm them.

## Output

Return purpose, component map, data/control flow, dependencies, configuration, tests, release path, risks, common change pattern, and suggested next step.
