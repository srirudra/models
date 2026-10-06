---
name: 'Implement PBI'
description: 'Have the Senior Engineer implement an approved PBI plan, consult Architect/Security/QA as needed, edit code, and run focused validation.'
agent: 'software-engineer'
model: ['Claude Sonnet 4.5 (copilot)', 'GPT-5 (copilot)']
tools: ['search', 'read', 'edit', 'execute', 'web', 'agent', 'todo']
---

Implement the approved PBI, bug fix, or technical task.

Start by reading the plan and nearest code/test anchors. Confirm the controlling code path, then make the smallest focused edit. Consult Architect for design uncertainty, Security Engineer for secrets/auth/data/release risk, and QA Engineer for test strategy or final validation. Run the cheapest focused validation after the first substantive edit and report changed files, validation results, residual risk, and follow-up tasks.