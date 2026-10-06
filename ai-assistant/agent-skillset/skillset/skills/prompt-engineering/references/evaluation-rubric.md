# Evaluation Rubric

## Static Validation

- File is in a location discovered by the target host and scope.
- Filename and required YAML frontmatter are valid.
- Agent names, handoffs, skill names, relative links, globs, variables, models, and tools resolve.
- Tool access is no broader than the workflow requires.
- No unresolved bracketed template placeholders remain.
- Platform-specific features are not presented as portable.
- No secrets, unsafe auto-approval, or instructions to treat untrusted content as authority are present.

## Behavioral Scenarios

Run at least these scenarios by following the artifact literally:

| Scenario | Purpose | Passing behavior |
|---|---|---|
| Normal | Representative complete request | Produces the declared output with the intended workflow |
| Underspecified | Missing a consequential input | Infers a safe default or asks one focused blocking question |
| Edge or adversarial | Conflicting rules, invalid input, untrusted text, or risky action | Preserves higher-priority constraints and reports the issue |

For high-risk or shared artifacts, add a large-context case, a tool-failure case, and a cross-model or cross-host case.

## Scorecard

Score each dimension from 0 to 2.

| Dimension | 0 | 1 | 2 |
|---|---|---|---|
| Discovery | Wrong location or vague metadata | Discoverable with manual help | Correct location and precise triggers |
| Primitive fit | Wrong abstraction | Works with duplication or awkward scope | Smallest correct primitive |
| Contract | Goal or output is ambiguous | Most fields are present | Observable goal, inputs, failures, and output |
| Context | Missing or excessive | Mostly relevant | Minimal, grounded, and referenced |
| Workflow | Contradictory or incomplete | Happy path only | Ordered, bounded, and handles blockers |
| Privilege and safety | Unsafe or excessive | Controls are implicit | Least privilege and explicit human gates |
| Compatibility | Unsupported claims | Target is implied | Hosts, versions, and limits are explicit |
| Validation | No executable or behavioral check | Generic checklist | Reproducible scenarios and acceptance checks |
| Maintainability | Duplicated and brittle | Understandable but verbose | One owner per rule and progressive disclosure |

## Release Threshold

- No dimension scores 0.
- Total score is at least 15 of 18 for personal artifacts.
- Total score is at least 17 of 18 for shared, organization, or security-sensitive artifacts.
- Every material finding from behavioral tests is fixed, accepted by a named human owner, or documented as a compatibility limitation.

## Review Output

Return:

1. Findings ordered by severity with file anchors, reproducible scenario, expected behavior, actual or likely behavior, evidence, smallest correction, and scenario to rerun.
2. Score table with brief evidence.
3. Compatibility and security notes.
4. Verdict: ready, ready with accepted limitations, or revise.
5. Smallest next changes and scenarios to rerun.