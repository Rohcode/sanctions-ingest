/**
 * Golden-vector generator for the Rust↔TS name-normalisation parity test.
 *
 * Imports the SERVER's dependency-free name-math (the single source of truth) and
 * emits a JSON fixture of {input, norm, phonetic} for a curated set of adversarial
 * inputs. The Rust parity test (tests/parity.rs) replays these inputs through the
 * Rust port and asserts byte-for-byte equality. If the server's normaliser ever
 * changes, regenerate this fixture and the Rust test will catch any drift.
 *
 * NOTE: this generator imports the TypeScript platform server's normaliser, which
 * lives in a separate private repository and is NOT part of this repo. The script
 * is kept here to document exactly how the fixture was produced and to make
 * regeneration reproducible for anyone who has that server checked out. It will
 * not run standalone; adjust the import below to point at your checkout.
 *
 * The committed `golden-vectors.json` IS the contract — `cargo test` replays it
 * and needs nothing from TypeScript.
 *
 * Run (with the server repo checked out alongside this one):
 *   npx tsx testdata/gen-golden-vectors.ts
 */
import { writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import {
  normalizeForSearch,
  phoneticTokens,
} from '../../server/src/services/sanctions/normalize.js';

// Curated adversarial inputs — each probes a parity hazard between the JS regex/
// Unicode engine and the Rust unicode crates.
const inputs: string[] = [
  // plain ASCII
  'John Smith',
  'JOHN   SMITH',
  '  John  Smith  ',
  // Latin diacritics (NFD strip)
  'Müller',
  'Ünsal',
  'Yevgeny Viktorovich Prigozhin',
  'José Núñez',
  'Łukasz Wałęsa',
  // hyphens / apostrophes / punctuation
  'Al-Bashir',
  "O'Brien",
  'Kim Jong-un',
  'IBN UMAR AL-MASRI',
  'Bin Laden, Osama',
  'A.B.C. Holdings (Pty) Ltd.',
  // transliteration variants
  'Mohammed Al-Rashid',
  'Muhammad',
  // German ß / capital ẞ (special lowercase, no decomposition)
  'Straße',
  'STRASSE',
  'ẞẞ', // ẞẞ → ßß
  // Turkish dotted capital İ (U+0130 → I + combining dot, dot stripped)
  'İstanbul',
  // Greek incl. final-sigma hazard (per-char lowercase must NOT apply final sigma)
  'ΟΔΥΣΣΕΥΣ', // ΟΔΥΣΣΕΥΣ
  // Cyrillic (kept, lowercased; soundex → empty after [^A-Z] strip)
  'Пригожин', // Григожин-ish
  // Arabic original script + harakat (combining marks → spaces)
  'أُسامة بن لادن',
  // CJK (kept; soundex empty)
  '金正恩',
  // digits + Arabic-Indic digits (Nd kept)
  'Unit 1000 ٠١٢',
  // numeric ref style
  '1000a',
  // empty / whitespace-only
  '',
  '   ',
  '...---...',
  // single-char tokens (filtered out of phonetic)
  'A Smith',
  'Al Smith',
  'John Jon',
];

const vectors = inputs.map((input) => ({
  input,
  norm: normalizeForSearch(input),
  phonetic: phoneticTokens(input),
}));

const outPath = join(dirname(fileURLToPath(import.meta.url)), 'golden-vectors.json');
writeFileSync(outPath, JSON.stringify(vectors, null, 2) + '\n', 'utf8');
console.log(`wrote ${vectors.length} golden vectors → ${outPath}`);
