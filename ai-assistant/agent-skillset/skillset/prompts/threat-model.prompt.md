---
name: 'Threat Model'
description: 'Ask Security Engineer to threat model a .NET/Azure feature, API, queue workflow, data flow, infrastructure change, or release plan.'
agent: 'security-engineer'
model: ['GPT-5.5 (copilot)', 'GPT-5 (copilot)', 'Claude Sonnet 4.5 (copilot)']
tools: ['search', 'read', 'web', 'agent']
---

Threat model the selected feature, service, API, queue workflow, infrastructure change, or release plan.

Identify assets, actors, trust boundaries, entry points, data flows, secrets, identities, permissions, abuse cases, likely attack paths, mitigations, residual risk, and verification. Consult Architect for boundary/data-flow clarity and QA Engineer for abuse-case test coverage when useful. Do not repeat secret values if encountered.