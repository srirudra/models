---
name: cloud-change-review
description: 'Review Azure, Terraform, IAM, networking, storage, service bus, secret, monitoring, cost, drift, rollback, and deployment changes before release.'
user-invocable: false
---

# Cloud Change Review

Use this skill for infrastructure, Terraform, Azure, and deployment topology changes.

## Procedure

1. Identify resources, environments, owners, dependencies, and deployment order.
2. Check identity, permissions, networking, secrets, Key Vault, managed identity, and policy compliance.
3. Review Terraform state impact, drift risk, import/move behavior, and plan/apply blast radius.
4. Check monitoring, alerting, diagnostics, cost impact, rollback, and smoke tests.
5. Define approval gates and release communications.

## Output

Return resource impact, risks, security concerns, state/drift notes, validation, rollback, monitoring, and go/no-go recommendation.
