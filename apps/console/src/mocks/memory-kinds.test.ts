import { MEMORY_KIND_NOTES } from "@zupflow/sdk";
import { describe, expect, it } from "vitest";
import { holdsEveryWord, KNOWN_MEMORY_KINDS, MAX_NOTE_TEXT, noteText, oneLine, searchWords } from "./memory-kinds";

// The backend's own cases for the text a note keeps and the words a search
// looks for, so the mock keeps and finds text as it does.

describe("oneLine", () => {
  it("keeps text to one line, without the characters that hide or reorder it", () => {
    expect(oneLine("a\r\nb\nc\rd\te")).toBe("a b c d e");
    expect(oneLine("a\u2028b\u0085c")).toBe("a b c");
    expect(oneLine("a\u000Bb\u000Cc\u2029d")).toBe("a b c d");
    expect(oneLine("no\u0000nul\u0007bell\u001b[0m")).toBe("nonulbell[0m");
    expect(oneLine("del\u007Fand\u0080c1\u009F")).toBe("delandc1");
    expect(oneLine("Prefers Português 🙂")).toBe("Prefers Português 🙂");
    expect(oneLine("tag\u{E0001}\u{E0041}\u{E0069}\u{E007F}s"), "tag characters").toBe("tags");
    expect(oneLine("\u202Aa\u202Bb\u202Cc\u202Dd\u202Ee"), "embeddings and overrides").toBe("abcde");
    expect(oneLine("\u2066a\u2067b\u2068c\u2069"), "isolates").toBe("abc");
    expect(oneLine("zero\u200Bwidth\u2060join\u2061\u2062\u2063\u2064ers")).toBe("zerowidthjoiners");
    expect(oneLine("\uFEFFmark\u180Eed\uFFF9\uFFFA\uFFFB")).toBe("marked");
    // Scripts and emoji need these, and they reorder nothing.
    for (const kept of ["👩\u200D💻 codes", "م\u200Cم", "a\u200Eb\u200Fc"]) {
      expect(oneLine(kept), JSON.stringify(kept)).toBe(kept);
    }
  });
});

describe("noteText", () => {
  it("keeps a note to one line of 1 to 500 characters, counted as characters", () => {
    expect(noteText("  Prefers answers\r\nin Portuguese.\u0007  ")).toBe("Prefers answers in Portuguese.");
    expect(noteText("\n\t \u0000"), "nothing left").toBeNull();
    expect(noteText("\u00A0\u3000\u200B\uFEFF"), "only spaces and hidden characters").toBeNull();
    expect([...(noteText("é".repeat(MAX_NOTE_TEXT)) ?? "")]).toHaveLength(MAX_NOTE_TEXT);
    expect(noteText("é".repeat(MAX_NOTE_TEXT + 1))).toBeNull();
    // An emoji is one character and two UTF-16 units.
    expect(noteText("🙂".repeat(MAX_NOTE_TEXT))).toBe("🙂".repeat(MAX_NOTE_TEXT));
    expect(noteText("🙂".repeat(MAX_NOTE_TEXT + 1))).toBeNull();
    // Trimmed after it's on one line, so spaces that were breaks go too.
    expect(noteText(`\r\n${"x".repeat(MAX_NOTE_TEXT)}\t\u2028`)).toBe("x".repeat(MAX_NOTE_TEXT));
  });

  it("is what the notes kind keeps, and says so when it refuses", () => {
    const notes = KNOWN_MEMORY_KINDS.get(MEMORY_KIND_NOTES);
    expect(notes?.text(" a\nb ")).toBe("a b");
    expect(notes?.textRule).toBe("Write a note of 1 to 500 characters on one line.");
  });
});

describe("searchWords", () => {
  it("looks for each word with a letter or a digit", () => {
    expect(searchWords("  fiscal\nYEAR -- 2026 ??? don't ")).toEqual(["fiscal", "YEAR", "2026", "don't"]);
    expect(searchWords(" ?! -- ")).toEqual([]);
    expect(searchWords("fi\u200Bnance\u3000²")).toEqual(["finance", "²"]);
  });
});

describe("holdsEveryWord", () => {
  const notes = [
    "Prefers answers in Portuguese.",
    "Invoices go to the finance team.",
    "The finance team meets on Mondays.",
    "Café com leite, não chá.",
    "Call or email, never text.",
  ];
  const found = (...words: string[]) => notes.filter((text) => holdsEveryWord(text, words));

  it("finds the notes holding every word, as whole words in any case and accent", () => {
    expect(found("FINANCE", "mondays"), "every word, in any case").toEqual(["The finance team meets on Mondays."]);
    expect(found("finance")).toHaveLength(2);
    expect(found("finance", "portuguese"), "every word, not any").toEqual([]);
    expect(found("cafe", "NAO"), "and any accent").toEqual(["Café com leite, não chá."]);
    expect(found("CAFÉ", "chá")).toEqual(["Café com leite, não chá."]);
    expect(found("OR"), "a word, never an operator").toEqual(["Call or email, never text."]);
    expect(found("NOT", "finance")).toEqual([]);
    expect(found('"finance"', "mondays*"), "quotes and stars are no syntax").toEqual([
      "The finance team meets on Mondays.",
    ]);
    expect(found("fin"), "whole words").toEqual([]);
  });

  it("reads a word with punctuation inside as a phrase of its parts, in order", () => {
    expect(holdsEveryWord("Don't send drafts.", ["don't"])).toBe(true);
    expect(holdsEveryWord("Don t send drafts.", ["don't"])).toBe(true);
    expect(holdsEveryWord("T don send drafts.", ["don't"])).toBe(false);
    expect(holdsEveryWord("Reports for 2026-q3 are due.", ["2026-Q3"])).toBe(true);
    // A word with a letter but nothing the index holds, as a circled letter, is left out; alone, it finds nothing.
    expect(holdsEveryWord("The finance team.", ["finance", "\u24B6"])).toBe(true);
    expect(holdsEveryWord("The finance team.", ["\u24B6"])).toBe(false);
  });

  it("folds diacritics only off Latin letters, and keeps other scripts' marks", () => {
    expect(holdsEveryWord("Résumé reviewed.", ["resume"])).toBe(true);
    expect(holdsEveryWord("Ölçüm tamam.", ["olcum"])).toBe(true);
    expect(holdsEveryWord("Привет всем.", ["ПРИВЕТ"])).toBe(true);
    expect(holdsEveryWord("Ελληνικά κείμενα.", ["ελληνικα"])).toBe(false);
  });

  it("finds nothing for no words at all", () => {
    expect(holdsEveryWord("Anything at all.", [])).toBe(false);
    expect(searchWords("?!")).toEqual([]);
  });

  it("reads a word as the tokenizer's Unicode tables do, folding case as it does", () => {
    expect(
      holdsEveryWord("\u03BB\u03CC\u03B3\u03BF\u03C2 notes", ["\u039B\u038C\u0393\u039F\u03A3"]),
      "a final sigma",
    ).toBe(true);
    expect(holdsEveryWord("5 \u00B5g a day", ["\u03BCg"]), "the micro sign").toBe(true);
    expect(holdsEveryWord("Over\u2013under", ["over"]), "a dash parts words").toBe(true);
    expect(holdsEveryWord("Use \u{F0001}codes", ["\u{F0001}codes"]), "private use is a letter").toBe(true);
    expect(holdsEveryWord("Code \uFDD0x", ["\uFDD0x"]), "and so is a character it doesn't know").toBe(true);
    expect(holdsEveryWord("Code \uFFFEx", ["x"]), "but the two it reads as a replacement character").toBe(true);
  });
});
