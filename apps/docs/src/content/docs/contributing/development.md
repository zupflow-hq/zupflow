---
title: Development
description: Work on Zupflow itself, with checks, the contract suite, Taskboard and these docs.
sidebar:
  order: 1
---

Read `AGENTS.md` in the repository first: it holds the ground rules every change follows.

## Checks

```bash
pnpm check        # lint and format, typecheck, tests and build for every package, Rust included
pnpm test         # Vitest and cargo test
pnpm format       # apply Biome fixes
```

CI runs `pnpm check`, the backend contract suite and the hosted Connect browser suite on every push and pull request.

## The contract comes first

When a screen needs data the API doesn't provide, change the contract first, then the SDK, then the mock (with a test), then the hook, then the page. `pnpm --filter @zupflow/contract generate` refreshes `openapi.json`, which also regenerates this site's [API reference](/reference/api/).

## The contract suite against the backend

```bash
CARGO_INCREMENTAL=0 pnpm contract:backend
```

Runs the shared suite, the same cases the mock passes, against the Rust server on a fresh fixture. `pnpm test:integration` runs it together with the Taskboard journeys and the hosted Connect browser tests.

## Taskboard

Taskboard is a fake MCP app with its own strict OAuth (metadata discovery, dynamic registration, PKCE, rotating refresh tokens) and three tools: `list_tasks` (read-only), `create_task` (writes) and `delete_task` (destructive). It lets you go through Connect, tool discovery, approvals and token refresh end to end, with no real service.

```bash
CARGO_INCREMENTAL=0 scripts/taskboard-dev.sh
```

It serves Taskboard and a development server, with **Taskboard live** ready in Sandbox, and prints the commands to start the Console and a browser that trusts Taskboard's certificate. In a Sandbox workspace, **Add installation**, choose **Taskboard live** and allow access. Then start a run with `call create_task {"title":"Hello"}`.

## These docs

The site is `apps/docs`, built with [Starlight](https://starlight.astro.build). Pages are Markdown or MDX in `src/content/docs`; anything interactive is a React component in `src/components`, used from MDX.

```bash
pnpm --filter @zupflow/docs dev     # http://localhost:5190
pnpm --filter @zupflow/docs build
```
