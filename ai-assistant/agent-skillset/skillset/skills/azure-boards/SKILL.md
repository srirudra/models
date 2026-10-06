---
name: azure-boards
description: >
  Read Azure Boards work items by ID or reference. Use whenever the user or context
  mentions PBI <ID>, PBI#<ID>, Product Backlog Item <ID>, WorkItem <ID>, Task <ID>,
  Bug <ID>, User Story <ID>, or any Azure DevOps work item identifier.
user-invocable: false
---

# Azure Boards — Work Item Reader

Organization: `{{AZURE_DEVOPS_ORG}}` (e.g. `https://dev.azure.com/YourOrg` — set via the installer's `-AzureDevOpsOrg` parameter or edit this file)

## When to Activate

Activate automatically when the conversation contains any of these patterns:

| Pattern | Example |
|---|---|
| `PBI <number>` | PBI 1234 |
| `PBI#<number>` | PBI#1234 |
| `Product Backlog Item <number>` | Product Backlog Item 1234 |
| `WorkItem <number>` | WorkItem 5678 |
| `Work Item <number>` | Work Item 5678 |
| `Task <number>` | Task 9012 |
| `Bug <number>` | Bug 3456 |
| `User Story <number>` | User Story 7890 |

## Procedure

1. Extract the numeric ID from the reference (strip all non-numeric characters).
2. Call `get_work_item` with `{ "id": <number> }`.
3. If the call succeeds, summarize: title, state, assigned-to, description, acceptance criteria, and linked items.
4. If the work item is not found or access is denied, report the error clearly and continue with whatever context is available.
5. Never block delivery work on a failed fetch — note the failure and proceed.

## Key MCP Tools (azure-devops server)

| Tool | Purpose |
|---|---|
| `get_work_item` | Fetch a single work item by integer ID |
| `list_work_items` | WIQL query — use for area/iteration/sprint searches |
| `get_work_item_comments` | Fetch discussion thread on a work item |
| `update_work_item` | Patch fields (requires Write scope PAT) |
| `create_work_item` | Create a new work item (requires Write scope PAT) |

## PAT Setup (one-time — fixes 400 Bad Request)

1. Go to: `{{AZURE_DEVOPS_ORG}}/_usersSettings/tokens`
2. New token → **Scopes**: Work Items → **Read** (add **Write** if agents should create/update items).
3. Copy the token and set it as a user environment variable:
   ```powershell
   [System.Environment]::SetEnvironmentVariable("AZURE_DEVOPS_PAT", "<your-pat>", "User")
   ```
4. Restart VS Code for the env var to be picked up by the MCP server process.

## Example WIQL for sprint queries

```wiql
SELECT [System.Id], [System.Title], [System.State]
FROM WorkItems
WHERE [System.TeamProject] = 'MyProject'
  AND [System.WorkItemType] = 'Product Backlog Item'
  AND [System.State] <> 'Closed'
ORDER BY [Microsoft.VSTS.Common.Priority]
```
