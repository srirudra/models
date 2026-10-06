---
name: 'Repository Investigation'
description: 'Use when exploring unfamiliar repositories, debugging issues, onboarding code areas, tracing behavior, or creating plans from local evidence.'
applyTo: '**/*.cs, **/*.config, **/*.json, **/*.xml, **/*.yml, **/*.yaml, **/*.md, **/*.sln, **/*.csproj'
---

# Repository Investigation Standards

- Start from the most concrete anchor: selected file, symbol, failing command, test, exception, PBI, deployment artifact, or configuration key.
- Read narrowly until you can state a falsifiable hypothesis, controlling code path, and cheapest discriminating check.
- Prefer nearby tests, dependency injection registrations, startup/config files, and call sites over broad repository tours.
- If a file only wires or forwards behavior, step once to the code that computes, mutates, persists, sends, or authorizes the behavior.
- Keep findings grounded in local evidence and separate assumptions from verified facts.
- When secrets are encountered, do not repeat their values; identify only type, location, and remediation path.
