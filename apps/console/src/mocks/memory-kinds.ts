/**
 * The memory kinds the mock knows, as the backend serves them: `notes`, the
 * kind this repository ships, with its description, the JSON Schema of its
 * settings and how they're checked, the text it keeps and how a search finds
 * it. A kind's key is an open string: a deployment may serve more kinds than
 * this repository ships. No dependencies on the HTTP layer, so the seed can
 * use them.
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
  /** A memory's text as the kind keeps it, or null for text it doesn't keep. */
  text(text: string): string | null;
  /** What a refusal of text says it keeps. */
  textRule: string;
  /** The most memories one agent keeps in one workspace in a type with these settings. */
  limit(settings: Record<string, unknown>): number;
  /** The words a list's `q` looks for: none when it has none to look for. */
  words(q: string): string[];
  /** Whether a memory's text holds every one of the words. */
  holds(text: string, words: readonly string[]): boolean;
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

/**
 * A type's `max_notes`, as the backend reads checked settings: a whole number
 * held to its bounds, and the default when it's missing or isn't one.
 */
function maxNotes(settings: Record<string, unknown>): number {
  const value = settings.max_notes;
  if (typeof value !== "number" || !Number.isInteger(value) || value < 0) return MAX_NOTES.default;
  return Math.min(Math.max(value, MAX_NOTES.min), MAX_NOTES.max);
}

// ---------------------------------------------------------------------------
// Text, as the backend keeps and finds it (`one_line`, `note_text` and
// `search_words`). Lengths are in characters, Unicode scalar values, never
// UTF-16 units.

/** The longest note, in characters. */
export const MAX_NOTE_TEXT = 500;

/** Line breaks other than `\r`, and tabs: each becomes a space. */
const SPACED = new Set(["\n", "\t", "\u000B", "\u000C", "\u0085", "\u2028", "\u2029"]);

/** A control character, general category Cc, as Rust's `char::is_control` has it. */
const isControl = (code: number) => code <= 0x1f || (code >= 0x7f && code <= 0x9f);

/**
 * An invisible character that can hide text or reorder it: a tag character, a
 * bidirectional embedding, override or isolate, a zero-width space, a word
 * joiner or invisible operator, a byte order mark, the Mongolian vowel
 * separator, or an interlinear annotation mark. Zero-width joiners and
 * non-joiners stay, as scripts and emoji need them, and so do the
 * left-to-right and right-to-left marks.
 */
function isHidden(code: number): boolean {
  return (
    (code >= 0xe0000 && code <= 0xe007f) ||
    (code >= 0x202a && code <= 0x202e) ||
    (code >= 0x2066 && code <= 0x2069) ||
    code === 0x200b ||
    (code >= 0x2060 && code <= 0x2064) ||
    code === 0xfeff ||
    code === 0x180e ||
    (code >= 0xfff9 && code <= 0xfffb)
  );
}

/**
 * Text on one line: each line break (`\r\n`, `\n`, `\r` and Unicode's) and
 * each tab becomes a space, and every other control character goes, with the
 * invisible characters `isHidden` lists.
 */
export function oneLine(text: string): string {
  const chars = [...text];
  let line = "";
  for (let i = 0; i < chars.length; i++) {
    const c = chars[i] ?? "";
    const code = c.codePointAt(0) ?? 0;
    if (c === "\r") {
      if (chars[i + 1] === "\n") i++;
      line += " ";
    } else if (SPACED.has(c)) {
      line += " ";
    } else if (!isControl(code) && !isHidden(code)) {
      line += c;
    }
  }
  return line;
}

/** Whitespace at either end, as Rust's `str::trim` takes it away: Unicode's `White_Space`. */
const EDGES = /^\p{White_Space}+|\p{White_Space}+$/gu;

/** Text without whitespace at either end. */
export const trimmed = (text: string) => text.replace(EDGES, "");

/** A note's text as kept: one line, trimmed, of 1 to 500 characters; null otherwise. */
export function noteText(text: string): string | null {
  const kept = trimmed(oneLine(text));
  const length = [...kept].length;
  return length >= 1 && length <= MAX_NOTE_TEXT ? kept : null;
}

/** The words a search looks for: split at whitespace, each holding a letter or a digit. */
export function searchWords(query: string): string[] {
  return oneLine(query)
    .split(/\p{White_Space}+/u)
    .filter((word) => /[\p{Alphabetic}\p{N}]/u.test(word));
}

// A search finds whole words in any case and accent, as SQLite's FTS5
// `unicode61` tokenizer with `remove_diacritics 2` does: a word is a run of
// letters, digits and private-use characters, which a combining diacritic
// continues; it's case-folded, and a diacritic over an ASCII letter goes.
// Each word sought is a phrase: its own words, one after another. The
// tokenizer reads characters by Unicode 6.1's tables and takes one they
// don't have for a letter, as here; a character assigned or recategorized
// since can still read differently.

/**
 * A letter, a digit, a private-use or unassigned character but the two the
 * tokenizer reads as a replacement character, or one of two Mongolian letters
 * that are marks since.
 */
const WORD_CHARACTER = /(?![\uFFFE\uFFFF])[\p{L}\p{N}\p{Co}\p{Cn}\u1885\u1886]/u;
const ASCII_LETTER_WITH_MARKS = /^[a-z][\u0300-\u036f]*$/;
/** Where the tokenizer's case folding isn't lower case: the final sigma, Greek letters' symbol forms, the micro sign, the long s. */
const FOLDS: ReadonlyMap<string, string> = new Map([
  ["\u00B5", "\u03BC"],
  ["\u017F", "s"],
  ["\u03C2", "\u03C3"],
  ["\u03D0", "\u03B2"],
  ["\u03D1", "\u03B8"],
  ["\u03D5", "\u03C6"],
  ["\u03D6", "\u03C0"],
  ["\u03F0", "\u03BA"],
  ["\u03F1", "\u03C1"],
  ["\u03F5", "\u03B5"],
  ["\u1E9B", "\u1E61"],
  ["\u1FBE", "\u03B9"],
]);
/** Cherokee letters, which the tokenizer gives no case. */
const CASELESS = /^[\u13A0-\u13F5]$/u;
/** A Latin letter whose diacritics the tokenizer keeps: a with dot above and macron. */
const KEEPS_MARKS = "\u01E1";

/** A combining diacritic the tokenizer keeps inside a word, then folds away: U+0300 to U+0331, as it lists them. */
function isDiacritic(code: number): boolean {
  if (code < 0x300 || code > 0x331) return false;
  const bit = code - 0x300;
  return bit < 32 ? ((0x08029fdf >>> bit) & 1) === 1 : ((0x000361f8 >>> (bit - 32)) & 1) === 1;
}

/** One character as a search compares it: case-folded, and an ASCII letter without its diacritics. */
function folded(c: string): string {
  if (CASELESS.test(c)) return c;
  const lower = FOLDS.get(c) ?? c.toLowerCase();
  if (lower === KEEPS_MARKS) return lower;
  const decomposed = lower.normalize("NFD");
  if (ASCII_LETTER_WITH_MARKS.test(decomposed)) return decomposed.charAt(0);
  return [...lower].length === 1 ? lower : c;
}

/** The words of a text, folded, as a search index holds them. */
function indexed(text: string): string[] {
  const words: string[] = [];
  let word: string | null = null;
  for (const c of text) {
    if (WORD_CHARACTER.test(c)) {
      word = (word ?? "") + folded(c);
    } else if (word !== null && !isDiacritic(c.codePointAt(0) ?? 0)) {
      words.push(word);
      word = null;
    }
  }
  if (word !== null) words.push(word);
  return words;
}

/**
 * Whether `text` holds every word, each a phrase of its own indexed words. A
 * word with none is left out, as the tokenizer leaves its phrase out of a
 * query, and with no word left nothing is found.
 */
export function holdsEveryWord(text: string, words: readonly string[]): boolean {
  const phrases = words.map((word) => indexed(word)).filter((phrase) => phrase.length > 0);
  if (phrases.length === 0) return false;
  const held = indexed(text);
  return phrases.every((phrase) => {
    for (let start = 0; start + phrase.length <= held.length; start++) {
      if (phrase.every((part, i) => held[start + i] === part)) return true;
    }
    return false;
  });
}

/** Every kind the mock can serve, by key. Which it does serve is the database's `memory_kinds`. */
export const KNOWN_MEMORY_KINDS: ReadonlyMap<string, KnownKind> = new Map([
  [
    MEMORY_KIND_NOTES,
    {
      description: NOTES,
      check: checkNotesSettings,
      text: noteText,
      textRule: `Write a note of 1 to ${MAX_NOTE_TEXT} characters on one line.`,
      limit: maxNotes,
      words: searchWords,
      holds: holdsEveryWord,
    },
  ],
]);
