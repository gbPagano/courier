---
icon: lucide/git-pull-request-create
---

# Pull requests

Pull requests should be narrow, reviewable, and tied to a clear issue or documentation need.

## Before you start

Open an issue first for behavior changes, new component APIs, or anything that changes the public configuration schema. This avoids spending time on a direction that may not fit the project.

Small documentation fixes, typos, and broken links can go straight to a pull request.

## Local checks

Run the relevant commands before opening a pull request:

```bash
cargo build
cargo check
cargo clippy --all-targets
cargo fmt
cargo test
cargo test <test_name>
```

Use `cargo check` for fast iteration and `cargo test` before requesting review.

CI runs `clippy` and `cargo test` on every push.

## Pull request checklist

- Keep the diff focused on one behavior, component, or documentation area.
- Add or update tests for behavior changes.
- Update documentation when user-facing configuration, runtime behavior, or CLI behavior changes.
- Keep generated files and unrelated cleanup out of the diff.
- Explain the motivation, approach, and trade-offs in the PR description.

## Commit hygiene

Prefer clear commit messages that describe the change in plain language. If a pull request has several exploratory commits, expect maintainers to ask for a squash before merge.

## Use of AI tools

AI-assisted coding is acceptable, but you are responsible for every line submitted. Review generated code, run tests, and be prepared to explain the implementation.
