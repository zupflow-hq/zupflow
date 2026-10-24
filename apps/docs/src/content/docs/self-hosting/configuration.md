---
title: Configuration
description: Every ZUPFLOW_* environment variable the server reads.
sidebar:
  order: 3
---

The server reads only environment variables. An invalid, empty or non-UTF-8 value **stops startup** rather than falling back to a default.

| Variable | Default | What it sets |
| --- | --- | --- |
| `ZUPFLOW_IDENTITY` | required | How the Console signs in: `oidc`, or `local` for one person on their own machine. |
| `ZUPFLOW_LISTEN` | `127.0.0.1:8080` | The address to listen on. `local` allows only loopback. |
| `ZUPFLOW_DATABASE` | `zupflow.db` | The SQLite file, created readable by its owner only. |
| `ZUPFLOW_LOG` | `info` | A tracing filter, like `zupflow_server=debug`. |
| `ZUPFLOW_SECRET_KEY` | generated with `local` | 32 bytes, base64. Required with `oidc`. Never change it. |
| `ZUPFLOW_PUBLIC_URL` | `http://localhost:{port}` | The origin browsers and providers reach the server at. OAuth redirect URIs are `{ZUPFLOW_PUBLIC_URL}/connect/oauth/callback`. |
| `ZUPFLOW_CONSOLE_ORIGIN` | none | The Console's origin, when it isn't the API's own. The only origin that gets CORS. |
| `ZUPFLOW_ALLOWED_ORIGINS` | none | Comma-separated extra origins a browser may call from. |
| `ZUPFLOW_OIDC_ISSUER` | with `oidc` | The `iss` of the provider's tokens. |
| `ZUPFLOW_OIDC_AUDIENCE` | with `oidc` | The Console's client ID. |
| `ZUPFLOW_OIDC_JWKS_URL` | discovery | The provider's keys, to skip discovery. |
| `ZUPFLOW_MODEL_PROVIDER` | none | `providers` to reach OpenAI, Anthropic and OpenRouter; `scripted` for local work only. Unset, runs fail `model_unavailable`. |

`ZUPFLOW_MODELS` and `ZUPFLOW_API_KEY_HASH_KEY` are retired: set, they stop startup with a message naming the replacement.

## The Console's build

| Variable | What it sets |
| --- | --- |
| `VITE_ZUPFLOW_API_URL` | The API's URL. Default `/api` on the Console's own origin. Unset in `pnpm dev`, the in-browser mock answers. |
| `VITE_ZUPFLOW_OIDC_ORIGINS` | The sign-in provider's issuer (and token endpoint origin, if different), allowed by the Console's CSP. |
