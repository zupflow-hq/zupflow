# Security

Zupflow holds other people's credentials and acts in their accounts, so we take reports seriously and answer them first.

## Reporting a vulnerability

Please report it privately, not in a public issue: use [**Report a vulnerability**](https://github.com/zupflow-hq/zupflow/security/advisories/new) on this repository's Security tab.

Tell us what you found, how to reproduce it, and what an attacker could do with it. We'll confirm we got it within three working days, keep you posted while we fix it, and credit you in the advisory unless you'd rather we didn't.

Please don't access or change data that isn't yours, and give us a reasonable time to ship a fix before you publish.

## Supported versions

Zupflow is in alpha. Fixes land on `main` and in the next release; there are no backports to older versions yet.

## Most useful to hear about

- One organization or workspace reaching another's data, credentials or runs
- Credentials (tokens, API keys, signing secrets) readable where they shouldn't be, or stored unsealed
- A tool call that runs without the grant, policy or approval it needs
- Forged or replayed webhooks, Connect sessions or OAuth callbacks that are accepted
- Server-side requests reaching addresses the outbound guard should refuse

Running a server with `ZUPFLOW_IDENTITY=local` (no sign-in, everyone is the owner) on a public address is a misconfiguration, not a vulnerability: local identity is for development on your own machine.
