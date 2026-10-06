---
name: threat-modeling
description: 'Threat model .NET and Azure systems. Use when reviewing APIs, queue workflows, data flows, identities, secrets, infrastructure, abuse cases, mitigations, and security verification.'
user-invocable: false
---

# Threat Modeling

Use this skill to review a design, feature, service, or infrastructure change before implementation or release.

## Procedure

1. Identify assets, actors, trust boundaries, identities, permissions, entry points, and data flows.
2. Enumerate abuse cases and likely attack paths, especially around auth, secrets, queues, storage, logs, and admin operations.
3. Rate risks by likelihood, impact, and exploitability.
4. Recommend mitigations that fit the existing .NET/Azure architecture.
5. Define security verification: tests, reviews, configuration checks, dependency checks, and release gates.
6. If secrets are found, do not repeat values; require rotation and secure storage.

## Output

Return assets, boundaries, abuse cases, risks, mitigations, verification, residual risk, and release blockers.
