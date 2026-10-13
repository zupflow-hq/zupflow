import { describe, expect, it } from "vitest";
import { contextWindowLabel, modelLabel, pricingLabel, routeName } from "./labels";

describe("model labels", () => {
  it("says how a model is reached", () => {
    expect(routeName("openai")).toBe("OpenAI");
    expect(routeName("openrouter")).toBe("via OpenRouter");
    const route = { connection_id: "mcn_x" as never, label: "Team key" };
    expect(modelLabel({ name: "GPT-5", route: { ...route, kind: "openai" } })).toBe("GPT-5 · OpenAI");
    expect(modelLabel({ name: "GPT-5", route: { ...route, kind: "openrouter" } })).toBe("GPT-5 · via OpenRouter");
  });

  it("formats sizes and prices", () => {
    expect(contextWindowLabel(null)).toBeNull();
    expect(contextWindowLabel(400_000)).toBe("400K tokens");
    expect(contextWindowLabel(1_048_576)).toBe("1.05M tokens");
    expect(pricingLabel({ input_per_million_usd: 1.25, output_per_million_usd: 10 })).toBe(
      "$1.25 in · $10 out per 1M tokens",
    );
    expect(pricingLabel({ input_per_million_usd: 0.3, output_per_million_usd: 0.075 })).toBe(
      "$0.30 in · $0.075 out per 1M tokens",
    );
  });
});
