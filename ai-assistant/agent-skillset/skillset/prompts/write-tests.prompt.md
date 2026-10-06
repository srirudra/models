---
name: 'Write Tests'
description: 'Ask the QA Engineer to design or implement focused .NET tests with NUnit, FluentAssertions, AutoFixture, Moq, integration, regression, or release validation coverage.'
agent: 'qa-engineer'
model: ['GPT-5.5 (copilot)', 'GPT-5 (copilot)', 'Claude Sonnet 4.5 (copilot)']
tools: ['search', 'read', 'edit', 'execute']
---

Create or update tests for the selected change.

Start by identifying the behavior under test and existing local test patterns. Then produce focused test cases for happy path, edge cases, dependency failures, regression risk, and security-sensitive behavior. Implement tests when context is sufficient, run the narrowest useful validation, and report any gaps that need manual verification.