---
name: 'Dependency Upgrade Review'
description: 'Review NuGet/package upgrades for .NET Framework/.NET Core compatibility, security, binding redirects, transitive dependencies, tests, and release risk.'
agent: 'principal-engineer'
model: ['GPT-5 (copilot)', 'Claude Sonnet 4.5 (copilot)']
tools: ['search', 'read', 'execute', 'web', 'agent']
---

Review the selected dependency upgrade or vulnerability remediation.

Check direct and transitive packages, target framework compatibility, binding redirects, API breaking changes, security motivation, Snyk/CVE context, test blast radius, deployment impact, and rollback path. Consult Security Engineer for vulnerability severity and exploitability, Senior Engineer for implementation mechanics, and QA Engineer for regression coverage. Return recommendation, required code/config changes, validation commands, and release risk.