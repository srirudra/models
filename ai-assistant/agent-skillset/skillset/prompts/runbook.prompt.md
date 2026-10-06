---
name: 'Runbook'
description: 'Create or update an operational runbook for .NET/Azure services, queue consumers, deployments, incidents, monitoring, rollback, and support handoff.'
agent: 'principal-engineer'
model: ['GPT-5 (copilot)', 'Claude Sonnet 4.5 (copilot)']
tools: ['search', 'read', 'edit', 'web', 'agent']
---

Create or update a runbook for the selected service, deployment, incident, or operational workflow.

Include service purpose, dependencies, dashboards/logs, alerts, common symptoms, triage checks, restart/replay/drain procedures, configuration and secret locations without secret values, escalation path, rollback, post-incident follow-up, and QA/release verification. Consult Architect for dependencies, Security Engineer for sensitive operations, and QA Engineer for verification evidence.