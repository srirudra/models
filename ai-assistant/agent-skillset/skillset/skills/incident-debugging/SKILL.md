---
name: incident-debugging
description: 'Debug .NET and Azure incidents, outages, queue failures, release regressions, logs, configuration issues, and operational symptoms. Use when triaging incidents or production-like failures.'
user-invocable: false
---

# Incident Debugging

Use this skill for production, staging, or lower-environment incidents.

## Procedure

1. State impact, affected users/systems, timeline, and current mitigation status.
2. Identify recent code, config, infrastructure, dependency, or deployment changes.
3. Build ranked hypotheses with a cheap, non-destructive check for each.
4. Inspect logs, configuration, queues, storage, health checks, and deployment history using least-privilege read-only access where possible.
5. If security, credential, auth, or data exposure is possible, involve Security Engineer immediately.
6. Separate mitigation from root-cause fix and from follow-up hardening.
7. Define validation for recovery, regression coverage, monitoring, and rollback.

## Output

Return impact, timeline, hypotheses, evidence, likely root cause, mitigation, permanent fix tasks, validation, communications notes, and residual risk.
