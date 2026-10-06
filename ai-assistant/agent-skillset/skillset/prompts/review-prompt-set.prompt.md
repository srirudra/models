---
name: 'review-prompt-set'
description: 'Run an independent read-only review of prompt files, instructions, custom agents, Agent Skills, model routing, or a complete prompt pack.'
argument-hint: 'Artifact paths; target hosts; intended users and behavior'
agent: 'prompt-reviewer'
---

# Review Prompt Set

Review the supplied prompt artifacts as an executable behavior system, not only as prose.

Check platform and location correctness, discovery metadata, primitive selection, requirement ownership, input and output contracts, context use, tool privilege, expert routing, untrusted-input handling, compatibility, references, failure behavior, and validation coverage.

Simulate normal, underspecified, and edge or adversarial scenarios. For every finding, include a reproducible scenario, expected behavior, actual or likely behavior, evidence, smallest correction, and scenario to rerun. Then include the nine-dimension scorecard, applicable release threshold, compatibility and security notes, test gaps, and verdict. Any zero or unresolved material finding requires `revise`. Do not edit files.