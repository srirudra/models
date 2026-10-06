---
name: 'Plan PBI'
description: 'Create a Principal Engineer PBI plan with specialist consultation, task breakdown, acceptance criteria, security review, QA strategy, and release plan.'
agent: 'principal-engineer'
model: ['GPT-5 (copilot)', 'Claude Sonnet 4.5 (copilot)']
tools: ['search', 'read', 'web', 'agent', 'todo']
---

Plan the provided PBI or feature request.

Include:

1. Problem statement and outcome.
2. Current codebase observations.
3. Assumptions, non-goals, and open questions.
4. Consultation summary from Senior Engineer, Architect, Security Engineer, and QA Engineer.
5. Implementation tasks assigned to Senior Engineer.
6. Acceptance criteria.
7. Test strategy and validation commands.
8. Release plan for TeamCity, Octopus, Azure, Terraform, rollback, and monitoring.
9. Risks and human decisions required.