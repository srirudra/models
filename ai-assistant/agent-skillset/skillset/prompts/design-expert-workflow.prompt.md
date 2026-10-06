---
name: 'design-expert-workflow'
description: 'Design or improve a multi-agent, multi-model, or mixture-of-experts-style Copilot workflow with specialist roles, routing, reconciliation, and human control points.'
argument-hint: 'Outcome; specialist domains; target host; tools; risk and cost constraints'
agent: 'prompt-architect'
---

# Design Expert Workflow

Create a practical expert-routing workflow for the stated outcome.

1. Separate application-level expert routing from any model provider's hidden mixture-of-experts implementation.
2. Define the parent decision owner and only the specialist roles that answer genuinely distinct questions.
3. For each role, specify trigger conditions, minimum context, allowed tools, expected evidence, output contract, and stop condition.
4. Decide which reviews can run independently in parallel and which require prior outputs.
5. Define reconciliation rules based on evidence, requirement priority, least privilege, and explicit human decisions rather than majority vote.
6. Use model fallback arrays only for availability failover. Use separate invocations when independent model-family review is required.
7. Before adding tools, hooks, lifecycle commands, destructive actions, or broader persistent authority, obtain explicit approval naming the path, scope, capability, and blast radius.
8. Implement the needed agents, prompts, instructions, or skill resources with no circular handoffs and one editing owner.
9. Test success, disagreement, specialist failure, missing context, and risky-action scenarios.

Report the routing design, files created or changed, tool and model rationale, failure behavior, validation evidence, and operating cost or latency tradeoffs.