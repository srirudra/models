---
name: pbi-planning
description: 'Plan enterprise .NET PBIs and epics. Use when decomposing features, writing acceptance criteria, assigning work to software or cloud engineers, coordinating architecture/security/QA review, or preparing release-ready task plans.'
user-invocable: false
---

# PBI Planning

Use this skill to turn an idea, PBI, bug, or epic into an implementation-ready plan.

## Procedure

1. Anchor the work in a user outcome, repository component, failing behavior, or release objective.
2. Read nearby code, tests, configuration, and docs only until the controlling path and major risks are clear.
3. Consult specialists for non-trivial work:
   - Software Engineer: implementation feasibility, TDD approach, and sequencing.
   - Architect: boundaries, contracts, resilience, data flow, and maintainability.
   - Security Engineer: threat model, secrets, identity, data protection, dependencies, and release controls.
   - QA Engineer: acceptance criteria, regression risk, test design, and release sign-off evidence.
4. Produce a task breakdown sized for implementation by a Software or Cloud Engineer according to the required skills.
5. Define acceptance criteria as observable behavior.
6. Include validation commands and release checks.

## Output

Return problem statement, assumptions, non-goals, specialist consultation summary, implementation tasks, acceptance criteria, test plan, release plan, risks, and human decisions required.
