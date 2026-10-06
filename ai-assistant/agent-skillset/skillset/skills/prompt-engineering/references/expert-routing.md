# Expert Routing

Use an expert ensemble when a task benefits from genuinely different lenses. This is application-level routing; it does not expose or control a model provider's internal mixture-of-experts implementation.

## Roles

| Role | Best question | Expected evidence |
|---|---|---|
| Prompt architect | Which primitive, scope, workflow, and output contract fit the goal? | Design choices and requirement mapping |
| Platform researcher | What does the current host and version support? | Official sources and compatibility limits |
| Domain specialist | Which non-obvious constraints and examples matter? | Repository or domain evidence |
| Security reviewer | Where can tools, secrets, untrusted input, or autonomy cause harm? | Threats, controls, and blocking findings |
| Evaluator | Does the artifact behave as promised across scenarios? | Rubric scores and reproducible failures |

Do not ask every expert the same broad question. That produces duplicated prose rather than useful diversity.

## Routing Strategy

1. The parent defines the contract and identifies uncertain dimensions.
2. Delegate only dimensions requiring independent expertise.
3. Run independent, read-only reviews in parallel when they do not depend on each other.
4. Require each expert to distinguish verified facts, assumptions, risks, and recommendations.
5. Reconcile by evidence and requirement priority, not majority vote.
6. Let one owner make edits. Reviewers report findings unless explicitly assigned an isolated file.
7. Re-run the evaluator after changes.

## Model Selection

- Use a strong reasoning model for architecture, conflict resolution, security, and ambiguous requirements.
- Use a fast model for bounded extraction, lint-like checks, and template filling.
- Use a model from a different family for adversarial review when the additional cost is justified.
- Use frontmatter fallback arrays only with model names available in the target Copilot installation. The first available model wins; an array is failover, not a simultaneous ensemble.
- For a true ensemble, invoke separate agents or review passes and reconcile their outputs explicitly.

## Delegation Contract

Every subagent request states:

1. Role and one focused objective.
2. Files or context to inspect.
3. Actions allowed and forbidden.
4. Questions to answer.
5. Evidence and output format required.
6. Stop condition and blockers.

## Reconciliation

Aggregate findings by requirement. For disagreements:

1. Check whether experts assumed different targets or versions.
2. Prefer official current documentation for platform facts.
3. Prefer local executable evidence for repository behavior.
4. Prefer least privilege and reversible behavior when evidence is incomplete.
5. Record any remaining human decision instead of hiding uncertainty.