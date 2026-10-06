---
name: 'Security Review'
description: 'Run a focused security review for .NET, Azure, configuration, secrets, Snyk, dependencies, authorization, infrastructure, or release changes.'
agent: 'security-engineer'
model: ['GPT-5.5 (copilot)', 'GPT-5 (copilot)', 'Claude Sonnet 4.5 (copilot)']
tools: ['search', 'read', 'execute', 'web']
---

Review the selected code, configuration, PBI, or release change for security risk.

Lead with findings ordered by severity. For each finding include component, evidence without secret values, impact, recommended remediation, verification, and release impact. Explicitly call out any hardcoded secret, credential rotation need, missing least privilege, auth issue, dependency risk, or operational control gap.