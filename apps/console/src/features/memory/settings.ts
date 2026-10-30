/**
 * A memory kind's settings, as the Console edits them: each one its kind's
 * JSON Schema describes, named by the schema's title and explained by its
 * description, shown as a field of its type and checked against its bounds
 * before anything is sent. The kind checks them again, and a setting it
 * refuses comes back named `settings.<key>`.
 */
import type { MemoryKind } from "@zupflow/sdk";
import { settingLabel } from "./labels";

export type SettingType = "integer" | "number" | "boolean" | "string" | "choice" | "json";

/** One setting of a kind. */
export interface SettingSpec {
  key: string;
  /** The schema's title, or the key in words when it has none. */
  label: string;
  /** The schema's description, shown as the field's help. */
  description: string | null;
  type: SettingType;
  /** Whether the schema requires it: one that isn't may be left empty, and the kind uses its default. */
  required: boolean;
  minimum: number | null;
  maximum: number | null;
  exclusiveMinimum: number | null;
  exclusiveMaximum: number | null;
  multipleOf: number | null;
  /** For a string: its shortest and longest length, in characters. */
  minLength: number | null;
  maxLength: number | null;
  default: unknown;
  /** For `choice`: the values its schema lists, all strings or all numbers. */
  options: (string | number)[];
}

/** A setting's value in the form: text for what's typed or chosen, true or false for a switch. */
export type SettingValue = string | boolean;

const record = (value: unknown): Record<string, unknown> =>
  value !== null && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : {};
const numberOrNull = (value: unknown) => (typeof value === "number" && Number.isFinite(value) ? value : null);

/** The values an `enum` lists when they're all strings, or all numbers; null otherwise. */
function enumOf(schema: Record<string, unknown>): (string | number)[] | null {
  const values = schema.enum;
  if (!Array.isArray(values) || values.length === 0) return null;
  if (values.every((v) => typeof v === "string")) return values;
  if (values.every((v) => typeof v === "number" && Number.isFinite(v))) return values;
  return null;
}

function typeOf(schema: Record<string, unknown>): SettingType {
  if (enumOf(schema)) return "choice";
  switch (schema.type) {
    case "integer":
      return "integer";
    case "number":
      return "number";
    case "boolean":
      return "boolean";
    case "string":
      return "string";
    default:
      return "json";
  }
}

/** The settings a kind's schema describes, in its order. */
export function settingSpecs(kind: Pick<MemoryKind, "settings_schema">): SettingSpec[] {
  const required = kind.settings_schema.required;
  return Object.entries(record(kind.settings_schema.properties)).map(([key, raw]) => {
    const schema = record(raw);
    // JSON Schema's older form marks `minimum` or `maximum` itself exclusive with `true`.
    const minimum = numberOrNull(schema.minimum);
    const maximum = numberOrNull(schema.maximum);
    const minimumExcluded = schema.exclusiveMinimum === true;
    const maximumExcluded = schema.exclusiveMaximum === true;
    const multipleOf = numberOrNull(schema.multipleOf);
    const description = typeof schema.description === "string" ? schema.description.trim() : "";
    return {
      key,
      label: settingLabel(key, schema),
      description: description || null,
      type: typeOf(schema),
      required: Array.isArray(required) && required.includes(key),
      minimum: minimumExcluded ? null : minimum,
      maximum: maximumExcluded ? null : maximum,
      exclusiveMinimum: minimumExcluded ? minimum : numberOrNull(schema.exclusiveMinimum),
      exclusiveMaximum: maximumExcluded ? maximum : numberOrNull(schema.exclusiveMaximum),
      multipleOf: multipleOf !== null && multipleOf > 0 ? multipleOf : null,
      minLength: numberOrNull(schema.minLength),
      maxLength: numberOrNull(schema.maxLength),
      default: schema.default,
      options: enumOf(schema) ?? [],
    };
  });
}

/** A setting's value as the form holds it: a type's own, else its default, else empty. */
export function formValue(spec: SettingSpec, value: unknown): SettingValue {
  const given = value === undefined ? spec.default : value;
  if (spec.type === "boolean") return given === true;
  if (given === undefined || given === null) return "";
  if (typeof given === "string") return given;
  if (typeof given === "number") return String(given);
  return JSON.stringify(given);
}

/** The form's values for a kind's settings, from a type's settings or the kind's defaults. */
export function formValues(specs: SettingSpec[], settings: Record<string, unknown> = {}) {
  return Object.fromEntries(specs.map((spec) => [spec.key, formValue(spec, settings[spec.key])]));
}

const formatted = (n: number) => n.toLocaleString("en", { maximumFractionDigits: 20 });

/** What a number setting's bounds allow, in words, for its field's help and its refusal. */
export function boundsRule(spec: SettingSpec): string {
  const what =
    spec.multipleOf !== null
      ? `a multiple of ${formatted(spec.multipleOf)}`
      : spec.type === "integer"
        ? "a whole number"
        : "a number";
  if (spec.minimum !== null && spec.maximum !== null) {
    return `Use ${what} from ${formatted(spec.minimum)} to ${formatted(spec.maximum)}.`;
  }
  const low =
    spec.minimum !== null
      ? `of at least ${formatted(spec.minimum)}`
      : spec.exclusiveMinimum !== null
        ? `greater than ${formatted(spec.exclusiveMinimum)}`
        : null;
  const high =
    spec.maximum !== null
      ? `${low ? "" : "of "}at most ${formatted(spec.maximum)}`
      : spec.exclusiveMaximum !== null
        ? `less than ${formatted(spec.exclusiveMaximum)}`
        : null;
  return `Use ${[what, [low, high].filter(Boolean).join(" and ")].filter(Boolean).join(" ")}.`;
}

/** What a string setting's lengths allow, in words; null when its schema sets none. */
export function lengthRule(spec: SettingSpec): string | null {
  const characters = (n: number) => `${formatted(n)} ${n === 1 ? "character" : "characters"}`;
  if (spec.minLength !== null && spec.maxLength !== null) {
    return `Use ${formatted(spec.minLength)} to ${characters(spec.maxLength)}.`;
  }
  if (spec.minLength !== null) return `Use at least ${characters(spec.minLength)}.`;
  if (spec.maxLength !== null) return `Use ${characters(spec.maxLength)} or fewer.`;
  return null;
}

/** Whether `n` is a whole number of `step`s, as far as floating point tells. */
function isMultiple(n: number, step: number): boolean {
  const steps = n / step;
  return Math.abs(steps - Math.round(steps)) < 1e-9;
}

/**
 * A setting's value to send, or why it can't be sent. An empty one the schema
 * doesn't require has no value: it's left out, and the kind uses its default.
 */
export function parsedSetting(
  spec: SettingSpec,
  value: SettingValue | undefined,
): { value: unknown; error?: undefined } | { value?: undefined; error: string } {
  if (spec.type === "boolean") return { value: value === true };
  const raw = typeof value === "string" ? value : "";
  const text = raw.trim();
  // A string's spaces are its own; any other field with only spaces is empty.
  if (spec.type === "string" ? raw === "" : text === "") {
    if (!spec.required) return { value: undefined };
    if (spec.type === "integer" || spec.type === "number") return { error: boundsRule(spec) };
    if (spec.type === "choice") return { error: "Choose one of the options." };
    if (spec.type === "json") return { error: "Write a JSON value." };
    return { error: "Fill this in." };
  }
  switch (spec.type) {
    case "integer":
    case "number": {
      const n = Number(text);
      const fits =
        Number.isFinite(n) &&
        (spec.type === "number" || Number.isInteger(n)) &&
        (spec.minimum === null || n >= spec.minimum) &&
        (spec.maximum === null || n <= spec.maximum) &&
        (spec.exclusiveMinimum === null || n > spec.exclusiveMinimum) &&
        (spec.exclusiveMaximum === null || n < spec.exclusiveMaximum) &&
        (spec.multipleOf === null || isMultiple(n, spec.multipleOf));
      return fits ? { value: n } : { error: boundsRule(spec) };
    }
    case "choice": {
      const chosen = spec.options.find((option) => String(option) === text);
      return chosen === undefined ? { error: "Choose one of the options." } : { value: chosen };
    }
    case "string": {
      const length = [...raw].length;
      const fits =
        (spec.minLength === null || length >= spec.minLength) && (spec.maxLength === null || length <= spec.maxLength);
      return fits ? { value: raw } : { error: lengthRule(spec) ?? "" };
    }
    case "json":
      try {
        return { value: JSON.parse(text) };
      } catch {
        return { error: "Write a JSON value." };
      }
  }
}

/** The settings to send: each parsed value, without those left empty for the kind's default. */
export function settingsToSend(specs: SettingSpec[], values: Record<string, SettingValue>): Record<string, unknown> {
  const settings: Record<string, unknown> = {};
  for (const spec of specs) {
    const { value } = parsedSetting(spec, values[spec.key]);
    if (value !== undefined) settings[spec.key] = value;
  }
  return settings;
}

/** A setting's default, as its field's help shows it. */
export function defaultLabel(spec: SettingSpec): string | null {
  if (spec.default === undefined) return null;
  if (typeof spec.default === "number") return formatted(spec.default);
  if (typeof spec.default === "boolean") return spec.default ? "On" : "Off";
  if (typeof spec.default === "string") return spec.default || null;
  return JSON.stringify(spec.default);
}

/** A stored setting's value as text, for a type whose kind this server doesn't serve. */
export function settingText(value: unknown): string {
  if (typeof value === "number") return formatted(value);
  if (typeof value === "string") return value;
  return JSON.stringify(value);
}
