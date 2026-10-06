# Knowledge Contract

## Repository Knowledge

Keep repository-specific facts in tracked repository documentation. The default PBI record is `docs/engineering/pbis/<pbi-id>/knowledge.md`. Promote durable facts to the repository's existing architecture, runbook, ADR, or engineering documentation when appropriate.

Record:

- Fact or lesson and why it matters.
- Evidence using repository-relative paths, tests, commands, ADRs, or issue identifiers.
- Scope and conditions where it applies.
- Date validated and PBI source.
- Superseded guidance when known.

## Personal Cross-Workspace Knowledge

Maintain:

- `~/.copilot/knowledge/repositories.md`: repository name/path, purpose, active or recent PBI ledger links, and last validation date.
- `~/.copilot/knowledge/engineering-lessons.md`: generalized reusable practices, failure patterns, validation approaches, and prompt-workflow improvements.

Do not copy repository code, customer data, environment details, internal endpoints, secrets, credentials, tokens, connection strings, sensitive findings, or proprietary business rules into cross-workspace knowledge. Store a pointer to the repository record instead.

Before promoting personal knowledge, Principal Engineer checks that the entry contains:

- No URI, remote repository URL, account name, tenant, subscription, hostname, environment identifier, or secret-like value.
- No customer, incident, payload, schema, endpoint, or proprietary implementation detail.
- No code block longer than ten lines; prefer a generalized explanation and repository-local citation.
- Only relative repository/PBI pointers, never credentials or authenticated links.
- Language that remains useful when the original organization, repository, and environment names are removed.

If any check fails, keep the lesson in repository knowledge or rewrite it at a safer level of abstraction.

## Update Rule

At PBI finalization, Principal Engineer reviews worker lessons, rejects unsupported conclusions, cites the source, updates repository knowledge, and promotes only lessons that are both reusable and safe across repositories. Mark stale or contradicted knowledge as superseded rather than silently deleting history.
