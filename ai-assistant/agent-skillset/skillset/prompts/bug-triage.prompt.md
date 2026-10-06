---
name: 'Bug Triage'
description: 'Have the Principal Engineer triage a bug, identify owning component, consult specialists, assign severity, and produce an implementation and validation plan.'
agent: 'principal-engineer'
model: ['GPT-5 (copilot)', 'Claude Sonnet 4.5 (copilot)']
tools: ['search', 'read', 'execute', 'web', 'agent', 'todo']
---

Triage the selected bug report, failing behavior, exception, log, or test failure.

Identify impact, severity, reproduction path, likely owning component, nearby code/test anchors, security/release implications, and the cheapest discriminating check. Consult Senior Engineer for fix feasibility, Architect for boundary/integration concerns, Security Engineer if secrets/auth/data risk is possible, and QA Engineer for regression coverage. Return severity, hypothesis, task breakdown, validation plan, and owner recommendation.