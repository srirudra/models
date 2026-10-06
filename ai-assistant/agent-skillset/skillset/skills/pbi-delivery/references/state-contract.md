# State Contract

## Status File

`status.md` is the resume anchor and contains:

```markdown
# <PBI ID>: <Title>

- Overall status: Intake | Research | Planned | In Progress | Verification | Principal Review | Blocked | Complete
- Accountable owner: Principal Engineer
- Workflow version: <TEAM_WORKFLOW version>
- Primary ledger repository: <path or name>
- Created: <ISO date>
- Last updated: <ISO date>
- Outcome: <observable business result>

## Affected Repositories
| Repository | Role in PBI | Ledger or cross-reference | Baseline reference | Required final evidence |
| ---------- | ----------- | ------------------------- | ------------------ | ----------------------- |

## Progress
| Work item | Owner | Status | Dependencies | Evidence | Next action |
|---|---|---|---|---|---|

## Decisions And Blockers
| ID | Type | Owner | Requested | Granted | Scope or action | Conditions and impact |
| -- | ---- | ----- | --------- | ------- | --------------- | --------------------- |

Types are `Scope`, `Approval`, `Exception`, `Tradeoff`, `Defect acceptance`, or `Blocker`. `Requested` and `Granted` use ISO timestamps; pending decisions use `Pending` for `Granted`. Record the human identity or accountable role, exact approved action and environment, conditions, expiry or follow-up, and linked work item. Chat approval is not sufficient until Principal Engineer records it here.

## Validation Snapshot
- Build: Not run | Pass | Fail
- Focused tests: Not run | Pass | Fail
- Broader tests: Not required | Not run | Pass | Fail
- Coverage: Not measured | Appropriate | Gap
- Security: Not required | Pending | Pass | Blocked
- QA: Pending | Pass | Blocked
- Cloud/release: Not required | Pending | Pass | Blocked

## Next Action
<one exact next action>
```

## State Transitions

| From | To | Required evidence |
| ---- | -- | ----------------- |
| Intake | Research | Identifier, outcome, affected repositories, and initial scope recorded |
| Research | Planned | Local evidence and required specialist findings reconciled |
| Planned | In Progress | Atomic work items are `Ready`, dependency order is valid, and required approvals are identified |
| In Progress | Verification | Implementation handbacks and focused checks exist for all in-scope items |
| Verification | Principal Review | Required build, tests, coverage, QA, security, architecture, cloud, and release evidence is present |
| Principal Review | Complete | Completion gate passes and final state and knowledge are written |
| Any non-complete state | Blocked | Blocker, owner, impact, decision needed, and safe parallel work recorded |
| Blocked | Prior active state | Blocking decision or evidence recorded and affected assumptions revalidated |
| Complete | Verification | Repository or external evidence invalidates a completion claim |
| Any state | Research or Planned | Scope, dependency, architecture, or hypothesis materially changes |

Do not skip a phase without recording why its entry and exit evidence is already satisfied.

## Plan File

`plan.md` records problem and outcome, verified current-system observations, assumptions, non-goals, specialist findings, reconciled decisions, architecture and security, dependency graph, acceptance criteria, test strategy, release and rollback, risks, and human decisions.

## Atomic Work Item

Each `work-items/<id>.md` contains:

- Identifier and title.
- Status: `Pending`, `Ready`, `In Progress`, `Blocked`, `Verification`, `Complete`, or `Cancelled`.
- Accountable role and required skills.
- Objective and explicit non-goals.
- Dependencies and affected paths.
- Acceptance criteria.
- Stakeholders to consult before implementation.
- TDD or verification approach.
- Required build, tests, coverage, security, cloud, and release evidence.
- Implementation summary and changed files.
- Validation commands and results.
- Lessons and unresolved risk.
- Cross-repository dependencies using `<repository>:<work-item-id>`.
- Evidence rows using the schema below.
- Reconciliation history and repair attempts.

An item is atomic when one worker can complete and verify it without an unrelated scope decision. Split items that cross independent ownership, can be released separately, or need different approval gates.

## Evidence Schema

Every acceptance, validation, approval, or exception record contains:

| Field | Requirement |
| ----- | ----------- |
| Evidence ID | Stable identifier referenced by status, work item, and completion report |
| Acceptance criterion or gate | Exact behavior or gate demonstrated |
| Type | Command, test, inspected artifact, plan, review, or human approval |
| Source | Command text, repository-relative artifact path, or decision ID |
| Context | Repository, commit/diff reference, environment, and relevant configuration |
| Result | Pass, Fail, Blocked, or Accepted with conditions |
| Timestamp | ISO date and time |
| Owner | Agent role or named human approver |
| Notes | Exit code, coverage rationale, limitation, expiry, or residual risk |

Do not store secret values or sensitive command output in evidence. Reference the secure source and redact values.

## Repair Loop

For each failed implementation or validation attempt, record the failure evidence, current hypothesis, attempted change, and next discriminating check. Do not repeat the same repair without new evidence. After two unsuccessful repairs with the same controlling hypothesis, or immediately when evidence falsifies the plan, return the item to `Research` or `Planned`, consult the relevant specialist, and mark it `Blocked` if no safe discriminating check remains.

## Ledger Reconciliation

When starting or resuming:

1. Inspect worktree status, current branch/commit, and history since `Last updated` using repository-appropriate non-destructive checks.
2. Compare changed, reverted, deleted, and generated files with every work-item implementation summary and evidence context.
3. Validate external dependencies, package/artifact versions, configuration, infrastructure plans, and approvals that may have expired.
4. Add a reconciliation entry to each affected work item with timestamp, discrepancy, evidence, and resolution.
5. Move a stale `Complete` item to `Verification`; move the overall PBI from `Complete` to `Verification` if any required claim is invalidated.
6. Update the validation snapshot and exact next action before implementation resumes.

Do not overwrite unrelated user changes while reconciling.

## Multi-Repository PBIs

1. Select one primary repository for the authoritative PBI ledger, preferring the repository that owns the business outcome or integration contract.
2. List every affected repository, role, baseline reference, required final evidence, and local PBI cross-reference in `status.md`.
3. Add a lightweight `docs/engineering/pbis/<pbi-id>.md` pointer in each secondary repository when repository policy permits; otherwise record the local path in the primary ledger.
4. Express dependencies as `<repository>:<work-item-id>`, including package publication, schema/contract ordering, compatibility windows, and rollout sequence.
5. Capture build and test evidence independently for each repository and end-to-end or contract evidence across repository boundaries.
6. Completion requires every affected repository to reach its required baseline, integration compatibility to pass, release order and rollback to be documented, and repository-specific knowledge to be updated locally.

## Completion File

`completion.md` contains:

- Outcome and scope delivered.
- Work-item final-state table and cancellation decision IDs.
- Changed files grouped by repository.
- Acceptance-criterion-to-evidence mapping with no unmapped criterion.
- Build, focused test, broader test, coverage, static analysis, and specialist evidence IDs.
- TDD evidence or Principal-approved exception decision ID.
- Deployment, migration, release, rollback, monitoring, and approval state.
- Accepted defects with severity, owner, rationale, expiry/follow-up, release impact, and sign-off date.
- Residual risks and follow-up work.
- Repository and personal knowledge updates.

## State Integrity

- Principal Engineer writes or reconciles overall status.
- Workers may update their assigned work-item file but must not close the PBI.
- Never mark evidence as passed without a command result, inspected artifact, or named human approval.
- When repository state conflicts with the ledger, treat the repository as evidence, report the discrepancy, and repair the ledger before continuing.
