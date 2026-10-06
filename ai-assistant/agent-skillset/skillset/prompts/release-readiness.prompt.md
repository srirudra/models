---
name: 'Release Readiness'
description: 'Assess release readiness for TeamCity, Octopus, Terraform, Azure, .NET configuration, rollback, smoke tests, monitoring, and go/no-go decisions.'
agent: 'principal-engineer'
model: ['GPT-5 (copilot)', 'Claude Sonnet 4.5 (copilot)']
tools: ['search', 'read', 'execute', 'web', 'agent', 'todo']
---

Assess release readiness for the selected change or release candidate.

Cover TeamCity build, artifact/versioning, Octopus variables and deployment steps, Terraform/Azure impact, .NET config transforms, secrets, smoke tests, monitoring, rollback, support handoff, and approval gates. Consult Security Engineer for release blockers and QA Engineer for verification evidence. Return go/no-go recommendation with blockers, required fixes, and post-release monitoring.