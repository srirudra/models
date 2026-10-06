---
name: dependency-upgrade
description: 'Review and implement NuGet or package upgrades for .NET Framework and .NET Core. Use for Snyk/CVE fixes, binding redirects, transitive dependencies, compatibility checks, and release risk.'
user-invocable: false
---

# Dependency Upgrade

Use this skill when upgrading packages or remediating dependency vulnerabilities.

## Procedure

1. Identify direct package, transitive dependency, current version, target version, and affected projects.
2. Confirm target framework compatibility and known breaking changes.
3. For .NET Framework, check packages.config, assembly binding redirects, app.config/web.config transforms, and runtime deployment behavior.
4. Review security advisory context and exploitability with Security Engineer when vulnerability-driven.
5. Update the smallest viable dependency set unless a coordinated upgrade is required.
6. Run focused compile/tests first, then broaden validation based on blast radius.
7. Define rollback and release notes.

## Output

Return upgrade rationale, affected projects, compatibility notes, required code/config changes, validation commands, release risk, and rollback path.
