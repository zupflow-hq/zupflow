/**
 * The memory kinds the mock knows, as the backend serves them: `notes`, the
 * kind this repository ships, with its description, the JSON Schema of its
 * settings and how they're checked. A kind's key is an open string: a
 * deployment may serve more kinds than this repository ships. No
 * dependencies on the HTTP layer, so the seed can use them.
 */
import { MEMORY_KIND_NOTES, type MemoryKind } from "@zupflow/sdk";

/** A type's settings as a kind checked them: its defaults filled in, or each refused setting named by its key. */
export type CheckedSettings =
  | { settings: Record<string, unknown>; refused?: undefined }
  | { settings?: undefined; refused: Record<string, string> };

export interface KnownKind {
  /** As `listMemoryKinds` describes it. */
  description: MemoryKind;
  check(settings: Record<string, unknown>): CheckedSettings;
}

/** A whole-number setting of `notes`: its bounds, its default, and what a refusal says. */
interface WholeSetting {
  min: number;
  max: number;
  default: number;
  refusal: string;
}

const RECALL_CHARACTERS: WholeSetting = {
  min: 0,
  max: 32_000,
  default: 8000,
  refusal: "Use a whole number from 0 to 32,000.",
};
const MAX_NOTES: WholeSetting = { min: 1, max: 1000, default: 200, refusal: "Use a whole number from 1 to 1,000." };

/** A `notes` type's settings when it leaves each one out, in key order as the backend writes them. */
export const NOTES_DEFAULT_SETTINGS = {
  max_notes: MAX_NOTES.default,
  recall_characters: RECALL_CHARACTERS.default,
} as const;

const NOTES: MemoryKind = {
  kind: MEMORY_KIND_NOTES,
  name: "Notes",
  description:
    "Short notes the agent saves with its tools, or people add, recalled at the start of each of its runs in the workspace. The agent searches for the ones left out.",
  settings_schema: {
    type: "object",
    properties: {
      recall_characters: {
        type: "integer",
        minimum: RECALL_CHARACTERS.min,
        maximum: RECALL_CHARACTERS.max,
        default: RECALL_CHARACTERS.default,
        description:
          "How many characters of notes a run starts with, counting each note's line. The oldest are left out first. 0 starts runs with none: the agent searches for them.",
      },
      max_notes: {
        type: "integer",
        minimum: MAX_NOTES.min,
        maximum: MAX_NOTES.max,
        default: MAX_NOTES.default,
        description: "The most notes one agent keeps in one workspace.",
      },
    },
    additionalProperties: false,
  },
  manage: { list: true, add: true, change: true, forget: true },
};

/** Each setting a whole number within its bounds, nothing else; one left out takes its default. */
function checkNotesSettings(values: Record<string, unknown>): CheckedSettings {
  const refused: Record<string, string> = {};
  for (const key of Object.keys(values)) {
    if (key !== "recall_characters" && key !== "max_notes") refused[key] = "Not a setting of notes.";
  }
  const whole = (key: string, setting: WholeSetting) => {
    if (!Object.hasOwn(values, key)) return setting.default;
    const value = values[key];
    if (typeof value === "number" && Number.isInteger(value) && value >= setting.min && value <= setting.max) {
      return value;
    }
    refused[key] = setting.refusal;
    return setting.default;
  };
  // In key order, as the backend writes them.
  const settings = {
    max_notes: whole("max_notes", MAX_NOTES),
    recall_characters: whole("recall_characters", RECALL_CHARACTERS),
  };
  return Object.keys(refused).length === 0 ? { settings } : { refused };
}

/** Every kind the mock can serve, by key. Which it does serve is the database's `memory_kinds`. */
export const KNOWN_MEMORY_KINDS: ReadonlyMap<string, KnownKind> = new Map([
  [MEMORY_KIND_NOTES, { description: NOTES, check: checkNotesSettings }],
]);
