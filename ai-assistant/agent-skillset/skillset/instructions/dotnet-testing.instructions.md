---
name: 'Dotnet Testing'
description: 'Use when writing or modifying .NET tests with NUnit, FluentAssertions, AutoFixture, Moq, integration tests, queue consumer tests, API tests, or regression coverage.'
applyTo: '**/*Tests*/**/*.cs, **/*Test*.cs'
---

# .NET Testing Standards

- Follow existing NUnit, FluentAssertions, AutoFixture, Moq, and local TestHelpers patterns.
- Name tests by behavior and observable outcome, not implementation detail.
- Prefer focused tests around the changed code path before adding broad integration coverage.
- Cover success, invalid input, null/empty values, boundary values, dependency failures, retries, and authorization/security-sensitive paths.
- Keep test data explicit enough to show intent; avoid large hidden fixtures unless the repository already uses them for that slice.
- When touching queue consumers, include idempotency, duplicate message, transient failure, and poison-message scenarios when applicable.
- When a test cannot be added cheaply, state the manual or integration validation that covers the risk.
