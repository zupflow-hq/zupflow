import { MODEL_PROVIDERS, type Model, type ModelProviderKind } from "@zupflow/sdk";

/**
 * How a model is reached, said the same way everywhere: "OpenAI" for a
 * provider's own models, "via OpenRouter" for one that serves other authors'.
 * The route is part of a model's identity, so it is always shown with it.
 */
export function routeName(kind: ModelProviderKind): string {
  const provider = MODEL_PROVIDERS[kind];
  return provider.routes_to_others ? `via ${provider.name}` : provider.name;
}

/** "GPT-5 · OpenAI", "GPT-5 · via OpenRouter". */
export function modelLabel(model: Pick<Model, "name" | "route">): string {
  return `${model.name} · ${routeName(model.route.kind)}`;
}

/** "400K tokens", or nothing when the provider doesn't say. */
export function contextWindowLabel(tokens: number | null): string | null {
  if (tokens === null) return null;
  if (tokens >= 1_000_000) return `${Number((tokens / 1_000_000).toFixed(2))}M tokens`;
  if (tokens >= 1_000) return `${Math.round(tokens / 1_000)}K tokens`;
  return `${tokens} tokens`;
}

/** "$1.25 in · $10 out per 1M tokens". */
export function pricingLabel(pricing: { input_per_million_usd: number; output_per_million_usd: number }): string {
  // Whole dollars stay whole; cents always show two places ($0.30, not $0.3).
  const usd = (n: number) =>
    Number.isInteger(n) ? `$${n}` : `$${Number(n.toFixed(2)) === n ? n.toFixed(2) : Number(n.toFixed(4))}`;
  return `${usd(pricing.input_per_million_usd)} in · ${usd(pricing.output_per_million_usd)} out per 1M tokens`;
}
