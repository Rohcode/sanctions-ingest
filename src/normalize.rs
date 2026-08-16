//! Byte-for-byte Rust port of the server's pure name-math
//! (the platform server's `sanctions/normalize.ts`).
//!
//! Parity is screening-correctness-critical: a name indexed here at ingest time
//! must normalise IDENTICALLY to the same name typed into the server at query
//! time, or the screen silently misses a match. The parity is proven by
//! tests/parity.rs against a golden-vector fixture generated from the TS source.
//!
//! Two hazards drove the implementation choices, both verified by golden vectors:
//!   1. Final sigma — JS `String.toLowerCase()` DOES apply the Greek final-sigma
//!      rule in context (ΟΔΥΣΣΕΥΣ → οδυσσευς, trailing ς). Per-char lowercasing
//!      would emit a context-free σ and diverge. So we lowercase at the STRING
//!      level (`str::to_lowercase`), which applies the same Unicode SpecialCasing
//!      Final_Sigma conditional as JS — done AFTER NFD+diacritic-strip so the
//!      context (word boundaries) matches the JS pipeline order.
//!   2. \p{L}/\p{N} vs Alphabetic — JS `\p{L}`/`\p{N}` are Unicode *general
//!      categories*. `char::is_alphabetic()` is the broader Alphabetic property
//!      (includes Other_Alphabetic combining marks). We therefore classify by
//!      true general category so e.g. Arabic harakat (Mn) become spaces, as in JS.

use unicode_general_category::{get_general_category, GeneralCategory};
use unicode_normalization::UnicodeNormalization;

/// True iff `c`'s Unicode general category is a Letter (\p{L}) or Number (\p{N}).
fn is_letter_or_number(c: char) -> bool {
    matches!(
        get_general_category(c),
        GeneralCategory::UppercaseLetter
            | GeneralCategory::LowercaseLetter
            | GeneralCategory::TitlecaseLetter
            | GeneralCategory::ModifierLetter
            | GeneralCategory::OtherLetter
            | GeneralCategory::DecimalNumber
            | GeneralCategory::LetterNumber
            | GeneralCategory::OtherNumber
    )
}

/// Port of `normalizeForSearch`:
///   NFD → strip Latin combining diacritics (U+0300..=U+036F) → per-char
///   lowercase → replace any non-(letter|number) with a space → collapse
///   whitespace runs → trim.
///
/// The JS does `replace(/[^\p{L}\p{N}\s]/gu,' ')` then `replace(/\s+/g,' ')`.
/// Since every non-(letter|number) char (whitespace included) ends up as a
/// space boundary that is then collapsed, mapping all non-alnum chars straight
/// to a single collapsed boundary yields an identical result.
pub fn normalize_for_search(input: &str) -> String {
    // NFD → drop Latin combining diacritics → string-level lowercase (applies the
    // context-sensitive final-sigma rule exactly as JS toLowerCase does).
    let stripped: String = input
        .nfd()
        .filter(|&c| !('\u{0300}'..='\u{036F}').contains(&c))
        .collect();
    let lowered = stripped.to_lowercase();

    // Map any non-(letter|number) char to a single collapsed space boundary; trim.
    let mut result = String::with_capacity(lowered.len());
    let mut started = false;
    let mut pending_space = false;
    for c in lowered.chars() {
        if is_letter_or_number(c) {
            if pending_space && started {
                result.push(' ');
            }
            result.push(c);
            started = true;
            pending_space = false;
        } else if started {
            pending_space = true; // boundary; trailing run dropped (trim)
        }
    }
    result
}

/// American Soundex for a single token — port of the TS `soundex`.
fn soundex(word: &str) -> String {
    let upper: Vec<char> = word
        .to_uppercase()
        .chars()
        .filter(|c| c.is_ascii_uppercase())
        .collect();
    if upper.is_empty() {
        return String::new();
    }
    fn code(c: char) -> char {
        match c {
            'B' | 'F' | 'P' | 'V' => '1',
            'C' | 'G' | 'J' | 'K' | 'Q' | 'S' | 'X' | 'Z' => '2',
            'D' | 'T' => '3',
            'L' => '4',
            'M' | 'N' => '5',
            'R' => '6',
            _ => '0',
        }
    }
    let mut result = String::new();
    result.push(upper[0]);
    let mut prev = code(upper[0]);
    let mut i = 1;
    while i < upper.len() && result.len() < 4 {
        let c = code(upper[i]);
        if c != '0' && c != prev {
            result.push(c);
        }
        prev = c;
        i += 1;
    }
    while result.len() < 4 {
        result.push('0');
    }
    result
}

/// Port of `phoneticTokens`: soundex per normalised word token longer than one
/// UTF-16 code unit (JS `String.length` semantics).
pub fn phonetic_tokens(name: &str) -> Vec<String> {
    normalize_for_search(name)
        .split(' ')
        .filter(|t| t.encode_utf16().count() > 1)
        .map(soundex)
        .collect()
}
