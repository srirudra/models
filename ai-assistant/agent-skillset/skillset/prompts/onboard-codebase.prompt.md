---
name: 'Onboard Codebase Area'
description: 'Create a concise engineering onboarding brief for a .NET codebase area, including architecture, ownership, risks, tests, config, and release notes.'
agent: 'architect'
model: ['GPT-5 (copilot)', 'Claude Sonnet 4.5 (copilot)']
tools: ['search', 'read', 'web', 'agent']
---

Create an onboarding brief for the selected codebase area, service, project, or feature.

Identify purpose, owning components, entry points, dependencies, data flow, configuration, external services, tests, release/deployment path, observability, known risks, and common change patterns. Consult Security Engineer for sensitive data or credential risks and QA Engineer for test coverage and verification guidance. Keep the output concise enough to reuse as context in a future implementation chat.