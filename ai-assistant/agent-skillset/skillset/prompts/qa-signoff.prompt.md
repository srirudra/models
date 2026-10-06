---
name: 'QA Sign-off'
description: 'Ask QA Engineer to run or design release sign-off, regression coverage, acceptance verification, defect reporting, and PASS/BLOCKED recommendation.'
agent: 'qa-engineer'
model: ['GPT-5.5 (copilot)', 'GPT-5 (copilot)', 'Claude Sonnet 4.5 (copilot)']
tools: ['search', 'read', 'edit', 'execute', 'web']
---

Perform QA sign-off for the selected PBI, implementation, bug fix, or release candidate.

Map acceptance criteria to tests and evidence. Run or propose the narrowest useful validation first, then broaden for regression or release risk. Report test scenarios, commands, observed results, defects, blockers, missing evidence, and final PASS/BLOCKED recommendation.