---
name: 'C# and .NET Engineering'
description: 'Use when editing C#, .NET Framework, .NET Core, WebApi/Owin, Autofac, MassTransit, MongoDB, Azure Storage, Azure Service Bus, csproj, app.config, web.config, or packages.config files.'
applyTo: '**/*.cs, **/*.csproj, **/*.config, **/packages.config, **/*.sln'
---

# C# and .NET Engineering Standards

- Match the existing project style, target framework, dependency versions, and nullable/async conventions before introducing new patterns.
- Prefer explicit, testable services registered through the existing dependency injection container.
- Keep WebApi/Owin controllers thin; put business behavior in services or managers already used by the codebase.
- Treat queue consumers as distributed systems code: validate idempotency, retries, poison-message behavior, logging, and dependency failure handling.
- Use existing logging conventions and never log secrets, tokens, connection strings, or sensitive payloads.
- For config changes, account for app.config/web.config transforms, deployment variables, binding redirects, and environment-specific overrides.
- Avoid broad framework upgrades, package churn, or cross-cutting refactors unless the task explicitly requires them.
