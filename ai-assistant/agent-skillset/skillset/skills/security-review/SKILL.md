---
name: security-review
description: 'Review .NET and Azure code for security. Use when checking hardcoded secrets, Snyk findings, authentication, authorization, dependency risk, queue security, storage access, or release controls.'
user-invocable: false
---

# Security Review

Use this skill for focused security review of code, configuration, infrastructure, release plans, or PBIs.

## Procedure

1. Identify trust boundaries, identities, data classification, and external dependencies.
2. Search nearby files for credentials, connection strings, tokens, certificates, auth bypasses, unsafe logging, and insecure defaults.
3. Review authentication, authorization, input validation, output encoding, dependency versions, and operational controls.
4. For Azure and deployment work, check Key Vault, managed identities, scoped permissions, Octopus sensitive variables, TeamCity secure parameters, and Terraform state risk.
5. If a hardcoded secret is found, do not repeat the value. Require human-owned rotation and replacement with a secure secret store.
6. Rank each finding and provide verification steps.

## Severity Guide

- Blocker: leaked production credential, auth bypass, remote code execution, or release control gap with immediate impact.
- High: exploitable data exposure, broad privilege, unsafe secret handling, or vulnerable dependency with reachable path.
- Medium: defense-in-depth gap, weak validation, excessive logging, or missing auditability.
- Low: documentation, hardening, or hygiene issue.

## Output

Lead with findings by severity, then remediation, verification, release impact, and residual risk.
