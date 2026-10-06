---
name: 'Team Consultation'
description: 'Have Principal Engineer coordinate Senior Engineer, Architect, Security Engineer, and QA Engineer perspectives for a plan, bug, design, release, or technical decision.'
agent: 'principal-engineer'
model: ['GPT-5 (copilot)', 'Claude Sonnet 4.5 (copilot)']
tools: ['search', 'read', 'web', 'agent', 'todo']
---

Run a structured engineering-team consultation for the selected topic.

Collect and reconcile perspectives from Senior Engineer, Architect, Security Engineer, and QA Engineer. Identify agreements, disagreements, risks, assumptions, blockers, and decisions needed from humans. End with a single recommended path, assigned follow-up tasks, and validation evidence.