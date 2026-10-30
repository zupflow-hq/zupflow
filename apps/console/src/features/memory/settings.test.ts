import { MEMORY_KIND_NOTES } from "@zupflow/sdk";
import { describe, expect, it } from "vitest";
import {
  boundsRule,
  defaultLabel,
  formValues,
  lengthRule,
  parsedSetting,
  type SettingSpec,
  settingSpecs,
  settingsToSend,
} from "./settings";

const notes = {
  kind: MEMORY_KIND_NOTES,
  settings_schema: {
    type: "object",
    properties: {
      recall_characters: {
        type: "integer",
        minimum: 0,
        maximum: 32_000,
        default: 8000,
        title: "Recall length",
        description: "How many characters of notes a run starts with.",
      },
      max_notes: {
        type: "integer",
        minimum: 1,
        maximum: 1000,
        default: 200,
        title: "Note limit",
        description: "The most notes one agent keeps in one workspace.",
      },
    },
  },
};
/** A kind this repository doesn't ship, as a deployment might serve it. */
const archive = {
  kind: "archive",
  settings_schema: {
    type: "object",
    properties: {
      similarity: { type: "number", minimum: 0, maximum: 1, default: 0.75, title: "Similarity threshold" },
      index_titles: { type: "boolean", default: true },
      language: { type: "string", enum: ["english", "german"], default: "english" },
      filters: { type: "object", default: { pinned: true } },
      prompt_prefix: { type: "string" },
    },
  },
};
/** Bounds and rules beyond a minimum and a maximum, and settings a schema requires. */
const strict = {
  kind: "archive",
  settings_schema: {
    type: "object",
    required: ["batch_size", "label"],
    properties: {
      weight: { type: "number", exclusiveMinimum: 0, maximum: 1 },
      ratio: { type: "number", minimum: 0, exclusiveMaximum: 1 },
      batch_size: { type: "integer", minimum: 0, maximum: 100, multipleOf: 5 },
      dimensions: { type: "integer", enum: [256, 768, 1536], default: 768 },
      legacy: { type: "number", minimum: 0, exclusiveMinimum: true },
      label: { type: "string", minLength: 2, maxLength: 20 },
      extra: { type: "object" },
    },
  },
};

function specOf(kind: typeof strict | typeof archive | typeof notes, key: string): SettingSpec {
  const spec = settingSpecs(kind).find((s) => s.key === key);
  if (!spec) throw new Error(`No setting ${key}`);
  return spec;
}

describe("a kind's settings", () => {
  it("come from its schema, in its order, with labels, bounds and defaults", () => {
    const specs = settingSpecs(notes);
    expect(specs.map((s) => [s.key, s.label, s.type, s.minimum, s.maximum, s.default])).toEqual([
      ["recall_characters", "Recall length", "integer", 0, 32_000, 8000],
      ["max_notes", "Note limit", "integer", 1, 1000, 200],
    ]);
    const [recall, kept] = specs;
    if (!recall || !kept) throw new Error("No notes settings");
    expect(recall.description).toBe("How many characters of notes a run starts with.");
    expect(kept.description).toBe("The most notes one agent keeps in one workspace.");
    expect(boundsRule(kept)).toBe("Use a whole number from 1 to 1,000.");
    expect(boundsRule(recall)).toBe("Use a whole number from 0 to 32,000.");

    expect(settingSpecs(archive).map((s) => [s.key, s.label, s.type])).toEqual([
      ["similarity", "Similarity threshold", "number"],
      ["index_titles", "Index titles", "boolean"],
      ["language", "Language", "choice"],
      ["filters", "Filters", "json"],
      ["prompt_prefix", "Prompt prefix", "string"],
    ]);
  });

  it("are named by their schema alone, the notes kind's as any other's: by title, else by key", () => {
    const untitled = {
      kind: MEMORY_KIND_NOTES,
      settings_schema: {
        type: "object",
        properties: {
          recall_characters: { type: "integer", title: "  ", description: " " },
          max_notes: { type: "integer", title: " Note limit ", description: " The most notes kept. " },
        },
      },
    };
    expect(settingSpecs(untitled).map((s) => [s.label, s.description])).toEqual([
      ["Recall characters", null],
      ["Note limit", "The most notes kept."],
    ]);
  });

  it("start from a type's values, or the kind's defaults", () => {
    const specs = settingSpecs(archive);
    expect(formValues(specs)).toEqual({
      similarity: "0.75",
      index_titles: true,
      language: "english",
      filters: '{"pinned":true}',
      prompt_prefix: "",
    });
    expect(formValues(settingSpecs(notes), { max_notes: 50 })).toEqual({ recall_characters: "8000", max_notes: "50" });
    expect(specs.map(defaultLabel)).toEqual(["0.75", "On", "english", '{"pinned":true}', null]);
  });

  it("are checked against their type and bounds before anything is sent", () => {
    const [recall, kept] = settingSpecs(notes);
    if (!recall || !kept) throw new Error("No notes settings");
    expect(parsedSetting(kept, "50")).toEqual({ value: 50 });
    expect(parsedSetting(kept, " 1000 ")).toEqual({ value: 1000 });
    for (const refused of ["0", "1001", "2.5", "lots"]) {
      expect(parsedSetting(kept, refused)).toEqual({ error: "Use a whole number from 1 to 1,000." });
    }
    expect(parsedSetting(recall, "0")).toEqual({ value: 0 });

    const [similarity, titles, language, filters] = settingSpecs(archive);
    if (!similarity || !titles || !language || !filters) throw new Error("No settings");
    expect(parsedSetting(similarity, "0.5")).toEqual({ value: 0.5 });
    expect(parsedSetting(similarity, "1.5")).toEqual({ error: "Use a number from 0 to 1." });
    expect(parsedSetting(titles, false)).toEqual({ value: false });
    expect(parsedSetting(language, "french")).toEqual({ error: "Choose one of the options." });
    expect(parsedSetting(filters, '{"pinned":false}')).toEqual({ value: { pinned: false } });
    expect(parsedSetting(filters, "{pinned")).toEqual({ error: "Write a JSON value." });
  });

  it("leave out what's left empty unless the schema requires it, so the kind uses its default", () => {
    const kept = specOf(notes, "max_notes");
    expect(kept.required).toBe(false);
    expect(parsedSetting(kept, "")).toEqual({ value: undefined });
    expect(parsedSetting(specOf(archive, "filters"), " ")).toEqual({ value: undefined });
    expect(parsedSetting(specOf(archive, "prompt_prefix"), "")).toEqual({ value: undefined });

    expect(
      settingsToSend(settingSpecs(archive), {
        similarity: "",
        index_titles: false,
        language: "german",
        filters: "",
        prompt_prefix: "Recall:",
      }),
    ).toEqual({ index_titles: false, language: "german", prompt_prefix: "Recall:" });

    const batch = specOf(strict, "batch_size");
    expect(batch.required).toBe(true);
    expect(parsedSetting(batch, "")).toEqual({ error: "Use a multiple of 5 from 0 to 100." });
    expect(parsedSetting(specOf(strict, "label"), "")).toEqual({ error: "Fill this in." });
  });

  it("hold numbers to exclusive bounds and steps, in either form of the schema", () => {
    const weight = specOf(strict, "weight");
    expect(boundsRule(weight)).toBe("Use a number greater than 0 and at most 1.");
    expect(parsedSetting(weight, "0")).toEqual({ error: "Use a number greater than 0 and at most 1." });
    expect(parsedSetting(weight, "1")).toEqual({ value: 1 });

    const ratio = specOf(strict, "ratio");
    expect(boundsRule(ratio)).toBe("Use a number of at least 0 and less than 1.");
    expect(parsedSetting(ratio, "1")).toEqual({ error: "Use a number of at least 0 and less than 1." });
    expect(parsedSetting(ratio, "0")).toEqual({ value: 0 });

    const batch = specOf(strict, "batch_size");
    expect(parsedSetting(batch, "35")).toEqual({ value: 35 });
    expect(parsedSetting(batch, "36")).toEqual({ error: "Use a multiple of 5 from 0 to 100." });

    const legacy = specOf(strict, "legacy");
    expect([legacy.minimum, legacy.exclusiveMinimum]).toEqual([null, 0]);
    expect(boundsRule(legacy)).toBe("Use a number greater than 0.");
    expect(parsedSetting(legacy, "0")).toEqual({ error: "Use a number greater than 0." });

    const step = { ...specOf(strict, "weight"), exclusiveMinimum: null, maximum: null, multipleOf: 0.1 };
    expect(parsedSetting(step, "0.3")).toEqual({ value: 0.3 });
  });

  it("offer a number enum as a choice, and send the number", () => {
    const dimensions = specOf(strict, "dimensions");
    expect(dimensions.type).toBe("choice");
    expect(dimensions.options).toEqual([256, 768, 1536]);
    expect(formValues([dimensions])).toEqual({ dimensions: "768" });
    expect(parsedSetting(dimensions, "1536")).toEqual({ value: 1536 });
    expect(parsedSetting(dimensions, "512")).toEqual({ error: "Choose one of the options." });
  });

  it("hold strings to their lengths, in characters", () => {
    const label = specOf(strict, "label");
    expect(lengthRule(label)).toBe("Use 2 to 20 characters.");
    expect(parsedSetting(label, "a")).toEqual({ error: "Use 2 to 20 characters." });
    expect(parsedSetting(label, "🙂🙂")).toEqual({ value: "🙂🙂" });
    expect(lengthRule(specOf(archive, "prompt_prefix"))).toBeNull();
  });
});
