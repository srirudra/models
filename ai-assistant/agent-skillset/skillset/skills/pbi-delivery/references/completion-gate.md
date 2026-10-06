# Completion Gate

Principal Engineer verifies every applicable item:

## Product And Scope

- The observable PBI outcome and all acceptance criteria have evidence.
- Every work item is complete, or cancellation has a `Scope` decision ID, named human owner, rationale, affected acceptance criteria, and impact.
- Every affected repository and cross-repository dependency has required local and integration evidence.
- Scope changes, assumptions, non-goals, and follow-ups are explicit.

## Engineering

- Actual changes were reviewed against the plan and repository conventions.
- Affected projects or solution build successfully.
- Focused tests pass; required integration, regression, or full-suite tests pass.
- Coverage is appropriate to risk and important new behavior is protected.
- Software work used red-green-refactor where practical; every exception has Principal Engineer approval recording refactoring cost, scope/risk/time tradeoff, and alternative verification.
- Configuration, compatibility, migrations, queues, retries, idempotency, and failure modes are handled as applicable.

## Specialist Evidence

- QA provides PASS, or every accepted defect records severity, named human owner, rationale, expiry/follow-up, release impact, affected acceptance criteria, and sign-off date.
- Security blockers, leaked secrets, auth gaps, and unsafe dependencies are resolved; exposed secrets have human-owned rotation.
- Architecture contracts, boundaries, resilience, and observability are acceptable.
- Cloud and release changes have plan evidence, approvals, rollback, monitoring, cost, state/drift, and environment sequencing.

## Finalization

- Documentation, operational notes, support handoff, and release notes are updated as needed.
- `completion.md` records changed files, decisions, evidence, residual risks, deployment and rollback state, and follow-ups.
- `knowledge.md` captures reusable repository lessons with citations.
- Generalized lessons and repository pointers are promoted to personal knowledge without confidential details.
- Every acceptance criterion maps to at least one valid evidence ID, and every required gate maps to current repository/environment evidence.
- Personal knowledge contains no URI, remote repository URL, account or environment identifier, secret-like value, customer data, proprietary code, or code block longer than ten lines. Use generalized wording and local repository/PBI pointers.

Any unresolved required item keeps the PBI `Blocked` or `In Progress`, not `Complete`.
