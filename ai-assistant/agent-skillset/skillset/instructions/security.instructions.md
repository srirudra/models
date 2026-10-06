---
name: 'Security Engineering'
description: 'Use when reviewing or editing security-sensitive .NET, Azure, configuration, secrets, authentication, authorization, dependency, Snyk, TeamCity, Octopus, Terraform, or release-management work.'
applyTo: '**/*.cs, **/*.config, **/*.json, **/*.xml, **/*.yml, **/*.yaml, **/*.ps1, **/*.tf'
---

# Security Engineering Standards

- Never introduce, preserve, print, or repeat hardcoded secrets. If a secret is discovered, refer to it by location and type only, and require human-owned rotation.
- Treat MongoDB credentials, Azure Storage account keys, Service Bus keys, SAS tokens, client secrets, certificates, and bearer tokens as sensitive.
- Prefer Azure Key Vault, managed identities, scoped service principals, Octopus sensitive variables, and TeamCity secure parameters over plaintext configuration.
- Validate authentication, authorization, tenancy, input validation, output encoding, logging, auditability, dependency vulnerabilities, and least privilege.
- Do not remove existing security controls without a replacement and a clear risk rationale.
- For release decisions, leaked or unrotated production credentials are blockers unless a human owner explicitly accepts the risk.
- Complement Snyk-specific rules with architecture and operational review; do not duplicate Snyk findings blindly.
