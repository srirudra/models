---
name: 'Code Review'
description: 'Run an engineering code review focused on bugs, correctness, maintainability, security, tests, release risk, and .NET/Azure production readiness.'
agent: 'principal-engineer'
model: ['GPT-5 (copilot)', 'Claude Sonnet 4.5 (copilot)']
tools: ['search', 'read', 'execute', 'web', 'agent']
---

Review the selected changes as Principal Engineer.

Prioritize findings over summary. Check correctness, edge cases, architecture boundaries, security, tests, configuration, observability, release impact, and rollback risk. Consult Security Engineer for sensitive paths and QA Engineer for missing verification when useful. Return findings ordered by severity with file anchors, remediation, tests needed, and residual risk.