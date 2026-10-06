---
name: 'Engineering Documentation'
description: 'Use when writing or editing ADRs, runbooks, release notes, PBI plans, QA sign-offs, incident reports, or engineering handoff documents.'
applyTo: '**/*.md'
---

# Engineering Documentation Standards

- Write for future maintainers and release/support engineers, not only the current conversation.
- Separate verified facts, assumptions, decisions, risks, and open questions.
- Include concrete validation evidence when making readiness or sign-off claims.
- For ADRs, include context, decision, options considered, consequences, and follow-up tasks.
- For runbooks, include observability, safe triage checks, mitigations, rollback, escalation, and verification.
- For release notes, include user impact, technical impact, configuration/infrastructure changes, validation, rollback, monitoring, and support notes.
- Never include secret values; reference secret locations and rotation/ownership only.
