---
icon: lucide/file-pen-line
---

# Documentation

Documentation issues and pull requests help keep Courier's configuration reference, architecture notes, examples, and contributor docs accurate.

## Issue template

Documentation issues should include:

- **Title**: the page or topic that needs attention.
- **Description**: what is missing, unclear, inconsistent, or outdated.
- **Related links**: links to the relevant docs pages, headings, source files, or examples.
- **Proposed change**: optional, but helpful when the fix is straightforward.
- **Checklist**: confirmation that the issue covers one documentation problem.

## Making documentation changes

The documentation site lives in `docs/` and is built with [Zensical](https://zensical.org).

Preview locally with:

```bash
uvx zensical serve
uvx zensical build --clean
```

If you prefer a regular install:

```bash
pip install zensical
zensical serve
```

`zensical.toml` is the source of truth for site navigation. When adding or moving a page, update the `nav` array and check that the sidebar, title, table of contents, and cross-links render correctly.

!!! note "Template overrides leak into `site/`"

    `docs/overrides/` holds Jinja template overrides used by the home hero.
    Zensical copies every non-Markdown file under `docs/` verbatim into the
    output, so a stray `site/overrides/home.html` ends up in the build. It is
    harmless, but the CI workflow strips it before deploy. Run
    `rm -rf site/overrides` after a local build if you want to mirror that behavior.

## Style

- Start each page with a short paragraph that says what the page covers.
- Prefer tables for reference material such as config fields, traits, and runtimes.
- Cross-link directly to the relevant page, for example `[Architecture](../architecture.md)`.
- Keep examples minimal and copy-pasteable.
