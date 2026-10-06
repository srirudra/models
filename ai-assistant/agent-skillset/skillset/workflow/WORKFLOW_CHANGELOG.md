# Workflow Changelog

## 1.0.0 - 2026-07-23

- Made Principal Engineer accountable for end-to-end PBI delivery.
- Added hidden Software Engineer and Cloud Engineer implementation workers.
- Hid Architecture, Security, QA, and Prompt Reviewer agents from the human picker.
- Added durable repository state, completion, evidence, reconciliation, multi-repository, and knowledge contracts.
- Added `/deliver-pbi` and `/pbi-status` as the human PBI commands.
- Shipped 26 slash prompts across PBI delivery, prompt authoring, and domain families; role behavior lives in the agents and skills the prompts route to.
- Enabled bounded nested subagent consultation and documented the acyclic graph.
- Added TDD exception governance, production and Terraform approval gates, repair-loop escalation, structured evidence, and cross-workspace knowledge safeguards.
