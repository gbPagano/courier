---
icon: lucide/bug
---

# Bug reports

Bug reports should give maintainers enough information to reproduce the failure and decide whether it belongs in Courier, a dependency, or the surrounding runtime environment.

## Before creating an issue

Search existing issues and recent pull requests first. If the same problem is already being discussed, add new information there instead of opening a duplicate.

### Reduce the reproduction

Start from the smallest `config.toml` or pipeline excerpt that still fails. Remove unrelated pipelines, credentials, endpoints, and transforms.

If the failure is runtime-only, include the command you ran and the relevant environment variables, redacted as needed.

### Capture logs

For runtime failures, include logs from:

```bash
RUST_LOG=courier=debug courier run --config config.toml
```

Do not include secrets, tokens, passwords, or full payloads unless they are synthetic.

## Issue template

A complete bug report includes:

- **Title**: a short description of the failure.
- **Context**: what you were trying to do.
- **Bug description**: what went wrong and why it looks like a Courier bug.
- **Configuration**: the smallest `config.toml` or JSON excerpt that reproduces it.
- **Command**: the command you ran.
- **Logs**: relevant output, preferably with `RUST_LOG=courier=debug`.
- **Version**: the Courier git SHA or version.
- **Checklist**: confirmation that you searched existing issues and removed unrelated configuration.

## Good reports

Good bug reports are specific. They show one failure, one expected behavior, and one path to reproduce.

If you found a workaround, include it. Workarounds help other users while the bug is being investigated.
