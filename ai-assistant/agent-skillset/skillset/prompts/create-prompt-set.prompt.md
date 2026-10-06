---
name: 'create-prompt-set'
description: 'Create a coherent set of GitHub Copilot instructions, prompts, custom agents, Agent Skills, and supporting resources for a workflow or engineering domain.'
argument-hint: 'Outcome; VS Code or Visual Studio; personal or repository scope; users; constraints'
agent: 'prompt-architect'
---

# Create Prompt Set

This VS Code entrypoint builds the requested prompt set as working customization files, not only as examples in chat. Its outputs can target Visual Studio, but Visual Studio should receive only the repository instructions and prompt files supported by the installed release.

1. Establish the outcome, target host, scope, consumers, constraints, existing artifacts, and acceptance criteria. Ask only questions whose answers change the architecture or security boundary.
2. Inspect local conventions and research current official platform documentation for capabilities that may have changed.
3. Select the smallest necessary combination of instructions, prompt files, Agent Skills, custom agents, hooks, or shared instruction files. Explain why each artifact has a distinct owner and activation path.
4. For VS Code and Visual Studio compatibility, keep shared behavior in supported repository instructions and prompt files; isolate VS Code-only agents, skills, and handoffs.
5. Before adding hooks, execute or terminal tools, destructive automation, networked lifecycle commands, or broader persistent authority, obtain explicit approval naming the path, scope, capability, and blast radius.
6. Implement the complete set in the correct locations with valid, least-privilege frontmatter and resolved references.
7. Validate static structure and simulate normal, underspecified, and edge or adversarial scenarios.
8. Obtain an independent prompt review for shared outputs or any new skill, agent, tool, model, handoff, hook, cross-host claim, or output contract; fix material findings and rerun validation.

Report changed files, the workflow they form, usage examples, validation evidence, compatibility limits, and remaining human decisions.