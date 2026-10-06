---
name: 'pbi-status'
description: 'Resume or report a Principal Engineer PBI from its durable repository ledger, including progress, evidence, blockers, pending decisions, and next work.'
argument-hint: 'PBI ID, repository, or status question'
agent: 'principal-engineer'
---

# PBI Status

Locate and validate the durable delivery ledger for the requested PBI. For every affected repository, inspect non-destructive worktree status and history since the last ledger update, compare changed files and external dependencies with work-item evidence, record discrepancies, and downgrade stale completion claims to Verification before reporting. Return current phase, work-item table, completed evidence, in-progress and pending items, blockers, required human decisions, validation state, knowledge updates, and the exact next action.

If asked to continue, resume from the first unblocked incomplete work item and retain Principal Engineer accountability through finalization.