# Zupflow

Zupflow is a standalone, open-source backend for adding personal agents to your
product. Create an isolated workspace for each of your customers, connect their
accounts, give an agent access to selected tools, and run it inside your own
experience.

This repository is the monorepo for the platform. Today it holds the developer
**Console** and the TypeScript **SDK**, running against an in-browser mock of
the API. The backend comes next; it must serve the same contracts.

```
apps/
  console/        Developer Console — Vite, React 19, TanStack Router + Query, shadcn/ui
packages/
  sdk/            @zupflow/sdk — wire types, HTTP transport, typed resource client
  brand/          B1 Precision brand kit (SVG/PNG/ICO) and usage notes
```

## Getting started

Requires Node 22+ and pnpm 10.

```bash
pnpm install
pnpm dev
```

The Console opens on <http://localhost:5180>. Choose **Continue as Jordan Lee**
to explore the seeded Northwind organization (a fictional product that embeds
Zupflow), or **Create a new account** to walk through onboarding from an empty
organization.

With no `VITE_ZUPFLOW_API_URL` set, every request is served by the mock API in
`apps/console/src/mocks` through a service worker. Data is kept in this
browser's localStorage; **Reset demo data** in the user menu reseeds it.

## Checks

```bash
pnpm check        # Biome lint + format, typecheck, tests and build for every package
pnpm test         # Vitest only
pnpm format       # Apply Biome fixes
```

CI runs `pnpm check` on every push and pull request.

## How the Console is built

```
@zupflow/sdk (transport + typed client)
  → features/queries.ts (TanStack Query hooks; cache keys and invalidation)
    → routes/** (pages)
```

- Pages never call `fetch` or the SDK directly; they use the hooks in
  `src/features/queries.ts`.
- Wire contracts stay `snake_case`, exactly as the API serves them.
- Workspace-scoped calls carry the workspace ID in the path, and every
  workspace cache key starts with `["ws", workspaceId]`.
- Generic controls come from shadcn/ui (`src/components/ui`, generated — don't
  hand-edit). Console-specific building blocks live in `src/components/app`.
- Every page handles loading (layout-stable skeletons), empty, error,
  permission-denied and destructive-confirmation states, and works at 375 px.
- The Console shows one environment at a time (`?env=sandbox`, retained on
  every navigation; `src/features/environment.ts`). Workspaces are listed and
  searched on the server, never loaded in full.

### The mock API

`src/mocks` is a faithful stand-in for the backend, not a fixture file. It
serves the contracts in `packages/sdk/src/types.ts` and enforces the API's
rules: membership before existence, environment-matched configuration with no
fallback, single-use Connect sessions, exact return-URL allowlists,
workspace-coherent grants, approvals only from API keys, and no Console
actions in Production. It also simulates runs. `src/mocks/handlers.test.ts`
pins those rules; the backend should pass the same cases.

Two pieces exist only because there is no backend yet, and are labelled as such:

- **Sign-in** is simulated in `src/features/auth/session.ts`. Production uses
  Clerk; that module is the seam.
- **OAuth consent** sends the browser to `/connect/:id/simulated-consent`
  instead of a real provider.

## License

[Apache-2.0](LICENSE).

## Brand

Ink `#10191D` and cyan `#35C8F5`. Sandbox and Production are first-class colors
(amber and cyan): an environment is always visible before anyone connects an
account or starts a run. See `packages/brand/USAGE.txt`.
