---
name: 'Incident Debug'
description: 'Coordinate production or lower-environment incident debugging for .NET/Azure services, queues, logs, configs, dependencies, and release regressions.'
agent: 'principal-engineer'
model: ['GPT-5 (copilot)', 'Claude Sonnet 4.5 (copilot)']
tools: ['search', 'read', 'execute', 'web', 'agent', 'todo']
---

Debug the selected incident, outage, regression, failing job, alert, exception, or operational symptom.

Start with impact, timeline, affected components, recent changes, hypotheses, and the cheapest non-destructive checks. Consult Senior Engineer for code-path debugging, Architect for dependency/failure-mode analysis, Security Engineer if secrets/auth/data exposure may be involved, and QA Engineer for reproduction and regression coverage. Return current diagnosis, evidence, likely root cause, mitigations, permanent fix tasks, validation, and communications notes.