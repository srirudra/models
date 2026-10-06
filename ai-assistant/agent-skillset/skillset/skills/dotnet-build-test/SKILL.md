---
name: dotnet-build-test
description: 'Build and test C#/.NET Framework or .NET Core solutions. Use when selecting focused validation commands, running NUnit tests, diagnosing compile failures, or verifying SchemeAPI-style projects.'
user-invocable: false
---

# .NET Build and Test

Use this skill to choose and run the cheapest validation that can falsify the current change.

## Procedure

1. Identify the touched project, nearest test project, target framework, and test runner pattern from the solution.
2. Prefer focused validation before full-solution validation.
3. For .NET Framework packages.config projects, consider Visual Studio/MSBuild, NuGet package restore, binding redirects, and app.config/web.config dependencies.
4. For NUnit tests, follow the repository's existing runner and naming conventions.
5. When a command fails, fix only the relevant local defect unless the failure proves the plan is wrong.
6. Report command, result, failure summary, and remaining risk.

## Validation Ladder

- Compile touched project.
- Run touched test fixture or category.
- Run nearest test project.
- Run full solution build or test suite when blast radius requires it.

## Output

Return the validation chosen, why it was cheapest, exact commands run, results, and follow-up needed.
