---
name: release-management
description: 'Plan and review TeamCity, Octopus, Terraform, Azure, cloud, rollback, monitoring, and release-readiness work. Use when preparing deployment plans or release decisions.'
user-invocable: false
---

# Release Management

Use this skill to prepare release-ready plans for enterprise .NET services.

## Procedure

1. Identify changed services, dependencies, infrastructure, configuration, and deployment environments.
2. Confirm build artifact creation, versioning, promotion, and environment-specific variables.
3. Review TeamCity build assumptions, Octopus deployment steps, Terraform impact, Azure resources, secrets, smoke tests, monitoring, and rollback.
4. Check for manual approvals, migration order, backward compatibility, queue draining, and operational support needs.
5. Treat unrotated leaked secrets, missing rollback, untested smoke checks, or unknown production blast radius as release blockers.

## Output

Return release scope, environment impact, deployment sequence, validation, rollback, monitoring, approvals, risks, and go/no-go recommendation.
