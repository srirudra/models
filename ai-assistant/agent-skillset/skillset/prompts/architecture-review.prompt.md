---
name: 'Architecture Review'
description: 'Ask the Architect to review .NET/Azure design, service boundaries, APIs, queues, resilience, data flow, and release impact before implementation.'
agent: 'architect'
model: ['GPT-5 (copilot)', 'Claude Sonnet 4.5 (copilot)']
tools: ['search', 'read', 'web', 'agent']
---

Review the selected PBI, design, code area, or proposed implementation as Architect.

Identify business capability, owning components, integration points, data flow, failure modes, deployment topology, and long-term maintainability concerns. Consult Security Engineer for trust-boundary or secrets concerns and QA Engineer for testability or release evidence concerns. Return recommendation, tradeoffs, rejected options, risks, validation strategy, and concrete tasks for Senior Engineer.