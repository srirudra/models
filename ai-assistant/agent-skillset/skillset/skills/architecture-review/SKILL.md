---
name: architecture-review
description: 'Review .NET and Azure architecture. Use when evaluating service boundaries, API contracts, queue design, resilience, data flow, infrastructure impact, or technical decisions before implementation.'
user-invocable: false
---

# Architecture Review

Use this skill to evaluate the shape of a proposed or existing solution.

## Procedure

1. Identify the business capability, owning service, dependencies, data flow, and operational boundary.
2. Review API contracts, queue semantics, persistence, configuration, deployment topology, and observability.
3. Check resilience concerns: retries, timeouts, idempotency, poison messages, circuit breaking, backoff, and partial failure.
4. Check maintainability concerns: cohesion, coupling, dependency injection, testability, versioning, and migration path.
5. Consult security and QA perspectives for trust boundaries and release evidence.
6. Recommend the simplest design that satisfies current requirements and leaves a clear path for known future needs.

## Output

Return context, recommendation, tradeoffs, risks, rejected options, validation strategy, and implementation tasks.
