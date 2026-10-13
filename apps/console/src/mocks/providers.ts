/**
 * The mock's model providers: what each one lists for a key, and whether it
 * accepts the key. Fixtures only: the mock never reaches a provider. The
 * backend asks the provider itself (decision 0049), through the safe outbound
 * path, and keeps its list for about ten minutes.
 */
import type { ModelPricing, ModelProviderKind } from "@zupflow/sdk";

/**
 * The text that makes a provider refuse a key, in the mock: a key containing
 * it (any case) is refused, as the echo server refuses a token. Every other
 * key of the right shape is accepted.
 */
export const REFUSED_KEY_MARKER = "invalid";

/** Whether the provider accepts this key, as the backend learns by listing its models with it. */
export function providerAcceptsKey(_kind: ModelProviderKind, key: string): boolean {
  return !key.toLowerCase().includes(REFUSED_KEY_MARKER);
}

export interface ProviderCatalogEntry {
  provider_model_id: string;
  name: string;
  author: string;
  context_window: number | null;
  supports_tools: boolean;
  pricing: ModelPricing | null;
}

const price = (input: number, output: number): ModelPricing => ({
  input_per_million_usd: input,
  output_per_million_usd: output,
});

const entry = (
  provider_model_id: string,
  name: string,
  author: string,
  context_window: number | null,
  supports_tools: boolean,
  pricing: ModelPricing | null = null,
): ProviderCatalogEntry => ({ provider_model_id, name, author, context_window, supports_tools, pricing });

/**
 * What each provider lists. OpenAI's and Anthropic's model lists name no
 * prices, so neither does the mock; OpenRouter's lists both prices and
 * context sizes.
 */
const CATALOGS: Record<ModelProviderKind, ProviderCatalogEntry[]> = {
  openai: [
    entry("gpt-5", "GPT-5", "openai", 400_000, true),
    entry("gpt-5-mini", "GPT-5 mini", "openai", 400_000, true),
    entry("gpt-5-nano", "GPT-5 nano", "openai", 400_000, true),
    entry("gpt-4.1", "GPT-4.1", "openai", 1_047_576, true),
    entry("gpt-4.1-mini", "GPT-4.1 mini", "openai", 1_047_576, true),
    entry("gpt-4o", "GPT-4o", "openai", 128_000, true),
    entry("gpt-4o-mini", "GPT-4o mini", "openai", 128_000, true),
    entry("o3", "o3", "openai", 200_000, true),
    entry("o4-mini", "o4-mini", "openai", 200_000, true),
    entry("text-embedding-3-large", "Text embedding 3 large", "openai", 8_191, false),
    entry("gpt-image-1", "GPT Image 1", "openai", null, false),
  ],
  anthropic: [
    entry("claude-sonnet-4-5", "Claude Sonnet 4.5", "anthropic", 200_000, true),
    entry("claude-haiku-4-5", "Claude Haiku 4.5", "anthropic", 200_000, true),
    entry("claude-opus-4-1", "Claude Opus 4.1", "anthropic", 200_000, true),
    entry("claude-opus-4-0", "Claude Opus 4", "anthropic", 200_000, true),
    entry("claude-sonnet-4-0", "Claude Sonnet 4", "anthropic", 200_000, true),
    entry("claude-3-7-sonnet-latest", "Claude Sonnet 3.7", "anthropic", 200_000, true),
    entry("claude-3-5-haiku-latest", "Claude Haiku 3.5", "anthropic", 200_000, true),
  ],
  openrouter: [
    entry("openai/gpt-5", "GPT-5", "openai", 400_000, true, price(1.25, 10)),
    entry("openai/gpt-5-mini", "GPT-5 mini", "openai", 400_000, true, price(0.25, 2)),
    entry("openai/gpt-5-nano", "GPT-5 nano", "openai", 400_000, true, price(0.05, 0.4)),
    entry("openai/gpt-4.1", "GPT-4.1", "openai", 1_047_576, true, price(2, 8)),
    entry("openai/gpt-4o-mini", "GPT-4o mini", "openai", 128_000, true, price(0.15, 0.6)),
    entry("openai/o3", "o3", "openai", 200_000, true, price(2, 8)),
    entry("openai/o4-mini", "o4-mini", "openai", 200_000, true, price(1.1, 4.4)),
    entry("anthropic/claude-sonnet-4.5", "Claude Sonnet 4.5", "anthropic", 1_000_000, true, price(3, 15)),
    entry("anthropic/claude-haiku-4.5", "Claude Haiku 4.5", "anthropic", 200_000, true, price(1, 5)),
    entry("anthropic/claude-opus-4.1", "Claude Opus 4.1", "anthropic", 200_000, true, price(15, 75)),
    entry("anthropic/claude-sonnet-4", "Claude Sonnet 4", "anthropic", 1_000_000, true, price(3, 15)),
    entry("anthropic/claude-3.5-haiku", "Claude Haiku 3.5", "anthropic", 200_000, true, price(0.8, 4)),
    entry("google/gemini-2.5-pro", "Gemini 2.5 Pro", "google", 1_048_576, true, price(1.25, 10)),
    entry("google/gemini-2.5-flash", "Gemini 2.5 Flash", "google", 1_048_576, true, price(0.3, 2.5)),
    entry("google/gemini-2.5-flash-lite", "Gemini 2.5 Flash Lite", "google", 1_048_576, true, price(0.1, 0.4)),
    entry("google/gemma-3-27b-it", "Gemma 3 27B", "google", 131_072, false, price(0.07, 0.26)),
    entry("x-ai/grok-4", "Grok 4", "x-ai", 256_000, true, price(3, 15)),
    entry("x-ai/grok-code-fast-1", "Grok Code Fast 1", "x-ai", 256_000, true, price(0.2, 1.5)),
    entry("meta-llama/llama-4-maverick", "Llama 4 Maverick", "meta-llama", 1_048_576, true, price(0.15, 0.6)),
    entry(
      "meta-llama/llama-3.3-70b-instruct",
      "Llama 3.3 70B Instruct",
      "meta-llama",
      131_072,
      true,
      price(0.13, 0.39),
    ),
    entry("mistralai/mistral-medium-3.1", "Mistral Medium 3.1", "mistralai", 131_072, true, price(0.4, 2)),
    entry("mistralai/devstral-medium", "Devstral Medium", "mistralai", 131_072, true, price(0.4, 2)),
    entry("mistralai/mistral-7b-instruct", "Mistral 7B Instruct", "mistralai", 32_768, false, price(0.03, 0.05)),
    entry("deepseek/deepseek-chat-v3.1", "DeepSeek V3.1", "deepseek", 163_840, true, price(0.2, 0.8)),
    entry("deepseek/deepseek-r1-0528", "DeepSeek R1 0528", "deepseek", 163_840, true, price(0.4, 1.75)),
    entry("qwen/qwen3-coder", "Qwen3 Coder", "qwen", 262_144, true, price(0.22, 0.95)),
    entry("qwen/qwen3-235b-a22b-2507", "Qwen3 235B A22B", "qwen", 262_144, true, price(0.13, 0.6)),
    entry("moonshotai/kimi-k2-0905", "Kimi K2", "moonshotai", 262_144, true, price(0.38, 1.52)),
    entry("z-ai/glm-4.5", "GLM 4.5", "z-ai", 131_072, true, price(0.35, 1.55)),
    entry("nousresearch/hermes-3-llama-3.1-405b", "Hermes 3 405B", "nousresearch", 131_072, false, price(1, 1)),
  ],
};

/** The provider's whole list, in its own order. */
export function providerCatalog(kind: ModelProviderKind): readonly ProviderCatalogEntry[] {
  return CATALOGS[kind];
}

/** One model the provider lists, by its exact ID. */
export function providerModel(kind: ModelProviderKind, providerModelId: string): ProviderCatalogEntry | undefined {
  return CATALOGS[kind].find((m) => m.provider_model_id === providerModelId);
}

/** Matches the provider's model ID, name or author, ignoring case. */
export function matchesProviderModel(model: ProviderCatalogEntry, q: string): boolean {
  const needle = q.trim().toLowerCase();
  if (!needle) return true;
  return [model.provider_model_id, model.name, model.author].some((v) => v.toLowerCase().includes(needle));
}
