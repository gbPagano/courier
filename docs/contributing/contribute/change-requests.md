---
icon: lucide/hand-platter
---

# Change requests

Change requests are for behavior changes, new component APIs, configuration changes, or improvements that affect how users operate Courier.

## Before creating an issue

Make sure the request is not a bug report. If Courier already promises the behavior and fails to provide it, use [Bug reports](bug-reports.md) instead.

### Describe the workflow

Explain what you are trying to do from the operator or component author's point of view. Include sample configuration when the request affects `config.toml`.

### Look for related work

Search existing issues, pull requests, and documentation pages. If a similar idea already exists, add your use case there.

### Consider compatibility

Courier is pre-1.0, but configuration changes still need to be deliberate. Call out whether the request would change existing behavior, break existing config files, or require migration notes.

## Issue template

A complete change request includes:

- **Title**: a short summary of the proposed change.
- **Context**: what you are trying to achieve.
- **Description**: the proposed behavior or API.
- **Related links**: issues, docs, examples, or similar implementations.
- **Use cases**: who benefits and how often.
- **Configuration shape**: optional, but strongly recommended for config changes.
- **Trade-offs**: compatibility, complexity, performance, or operational impact.
- **Checklist**: confirmation that the request is scoped to one idea.

## How requests are reviewed

Maintainers may ask for more detail, suggest a smaller version, move the idea into a later milestone, or close it if it is outside the project's current direction.
