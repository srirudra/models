---
name: 'DevOps and Release Management'
description: 'Use when planning or editing TeamCity, Octopus, Terraform, Azure, GitVersion, deployment scripts, config transforms, build pipelines, release notes, rollback plans, or operational readiness.'
applyTo: '**/devops/**, **/*.tf, **/GitVersion.yml, **/*.yml, **/*.yaml, **/*.ps1, **/*.csproj, **/*.config'
---

# DevOps and Release Management Standards

- Keep build, artifact, infrastructure, application configuration, and deployment variables consistent across TeamCity, Octopus, Terraform, and Azure.
- Prefer immutable build artifacts promoted between environments rather than rebuilding per environment.
- Use least-privilege identities, Key Vault-backed secrets, Octopus sensitive variables, and TeamCity secure parameters.
- Include rollback, smoke test, monitoring, alerting, and support handoff steps for release-impacting changes.
- For .NET Framework deployments, verify config transforms, binding redirects, app settings, connection strings, service startup, and queue consumer hosting.
- For Terraform changes, call out state impact, drift risk, plan/apply ordering, and environment blast radius.
- Do not perform production-impacting commands without explicit human approval.
