---
name: 'Terraform Cloud Review'
description: 'Review Terraform, Azure, cloud infrastructure, IAM, networking, secrets, drift, state, rollout, rollback, and release impact.'
agent: 'principal-engineer'
model: ['GPT-5 (copilot)', 'Claude Sonnet 4.5 (copilot)']
tools: ['search', 'read', 'execute', 'web', 'agent']
---

Review the selected Terraform, Azure, or cloud infrastructure change.

Assess resource impact, identity and access, networking, secrets, state changes, drift risk, environment promotion, deployment ordering, rollback, monitoring, cost, and blast radius. Consult Architect for topology, Security Engineer for least privilege and secrets, and QA Engineer for release verification. Return blockers, required changes, validation commands, and go/no-go recommendation.