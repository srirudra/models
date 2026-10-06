---
name: 'Test Strategy'
description: 'Ask QA Engineer to design a layered test strategy for .NET APIs, queue consumers, persistence, integrations, security-sensitive paths, and releases.'
agent: 'qa-engineer'
model: ['GPT-5.5 (copilot)', 'GPT-5 (copilot)', 'Claude Sonnet 4.5 (copilot)']
tools: ['search', 'read', 'edit', 'execute', 'web']
---

Design a test strategy for the selected PBI, feature, service, bug, or release.

Map risks to test layers: unit, component, integration, contract, regression, exploratory, security, performance, smoke, and rollback validation. Identify existing tests to reuse, new tests to add, commands to run, manual checks, blockers, and evidence required for sign-off.