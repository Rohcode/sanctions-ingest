//! DFAT Consolidated List parser (calamine, memory-safe, cells-as-text).
//!
//! Maps the full documented 20-column schema (per "Guide to Australia's
//! Consolidated List"), groups rows by suffix-stripped Reference (1000a → 1000),
//! and derives allNamesNorm/allNamesPhonetic with the SAME logic as the server
//! (normalize.rs, parity-tested). Every cell is read as text — no formula
//! evaluation, no macro engine — so a hostile spreadsheet is inert.

use crate::normalize::{normalize_for_search, phonetic_tokens};
use calamine::{open_workbook_auto, Data, Reader};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;

/// Pre-parse caps: reject oversized / over-long inputs before any work.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_DATA_ROWS: usize = 20_000;

#[derive(Debug, Serialize)]
pub struct NameEntry {
    pub name: String,
    pub name_type: String,
    pub alias_strength: Option<String>,
    pub script: bool, // true ⇒ "original script" (non-Latin) name
}

#[derive(Debug, Serialize)]
pub struct DesignatedPerson {
    pub r#ref: String,
    pub entry_type: String, // person | entity | vessel
    pub primary_name: String,
    pub all_names: Vec<String>,
    pub all_names_norm: Vec<String>,
    pub all_names_phonetic: Vec<String>,
    pub names: Vec<NameEntry>,
    pub dates_of_birth: Vec<String>,
    pub places_of_birth: Vec<String>,
    pub citizenships: Vec<String>,
    pub addresses: Vec<String>,
    pub additional_info: String,
    pub listing_info: String,
    pub imo_number: String,
    pub committees: String,
    pub control_date: String,
    pub instrument_of_designation: String,
    pub targeted_financial_sanction: bool,
    pub travel_ban: bool,
    pub arms_embargo: bool,
    pub maritime_restriction: bool,
}

/// The documented DFAT column headers (row 1).
const COLS: &[&str] = &[
    "Reference",
    "Name of Individual or Entity",
    "Type",
    "Name Type",
    "Alias Strength",
    "Date of Birth",
    "Place of Birth",
    "Citizenship",
    "Address",
    "Additional Information",
    "Listing Information",
    "IMO Number",
    "Committees",
    "Control Date",
    "Instrument of Designation",
    "Targeted Financial Sanction",
    "Travel Ban",
    "Arms Embargo",
    "Maritime Restriction",
];

/// Every cell read as text — Display-free, formula-inert.
fn cell_text(d: &Data) -> String {
    match d {
        Data::String(s) => s.trim().to_string(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => {
            if f.fract() == 0.0 {
                format!("{}", *f as i64)
            } else {
                f.to_string()
            }
        }
        Data::Bool(b) => {
            if *b {
                "TRUE".to_string()
            } else {
                "FALSE".to_string()
            }
        }
        Data::DateTime(dt) => dt
            .as_datetime()
            // NaiveDateTime Display is "YYYY-MM-DD HH:MM:SS"; keep the date part.
            // (chrono's `format` needs a feature calamine doesn't enable.)
            .map(|d| d.to_string().split(' ').next().unwrap_or("").to_string())
            .unwrap_or_default(),
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
        Data::Error(_) | Data::Empty => String::new(),
    }
}

fn cell_bool(d: &Data) -> bool {
    match d {
        Data::Bool(b) => *b,
        _ => cell_text(d).eq_ignore_ascii_case("true"),
    }
}

/// Strip the trailing alias-suffix letters from a Reference (1000a → 1000).
fn group_key(reference: &str) -> String {
    reference
        .trim()
        .trim_end_matches(|c: char| c.is_ascii_alphabetic())
        .to_string()
}

fn map_entry_type(raw: &str) -> String {
    match raw.trim().to_lowercase().as_str() {
        "individual" | "person" => "person".to_string(),
        "entity" => "entity".to_string(),
        "vessel" => "vessel".to_string(),
        other => other.to_string(),
    }
}

/// Split a multi-value cell on commas (used for DOB years and citizenships, which
/// are comma-delimited lists). Place-of-birth and address are NOT split — they
/// contain commas as part of a single value.
fn split_commas(s: &str) -> Vec<String> {
    s.split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn one_if_present(s: &str) -> Vec<String> {
    let t = s.trim();
    if t.is_empty() {
        vec![]
    } else {
        vec![t.to_string()]
    }
}

fn dedup_nonempty(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for it in items {
        if !it.is_empty() && seen.insert(it.clone()) {
            out.push(it);
        }
    }
    out
}

#[derive(Default)]
struct Group {
    r#ref: String,
    entry_type: String,
    primary_name: String,
    names: Vec<NameEntry>,
    dates_of_birth: Vec<String>,
    places_of_birth: Vec<String>,
    citizenships: Vec<String>,
    addresses: Vec<String>,
    additional_info: String,
    listing_info: String,
    imo_number: String,
    committees: String,
    control_date: String,
    instrument_of_designation: String,
    targeted_financial_sanction: bool,
    travel_ban: bool,
    arms_embargo: bool,
    maritime_restriction: bool,
    has_primary: bool,
}

#[derive(Debug)]
pub struct ParseError(pub String);
impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ParseError {}

/// Parse the XLSX at `path` into grouped DesignatedPerson records.
/// `rows` returns the number of data rows consumed (for the audit event).
pub fn parse_consolidated_list(path: &Path) -> Result<(Vec<DesignatedPerson>, usize), ParseError> {
    let mut wb =
        open_workbook_auto(path).map_err(|e| ParseError(format!("cannot open workbook: {e}")))?;

    // Prefer the documented sheet name; fall back to the first sheet.
    let sheet = if wb.sheet_names().iter().any(|s| s == "Consolidated List") {
        "Consolidated List".to_string()
    } else {
        wb.sheet_names()
            .first()
            .cloned()
            .ok_or_else(|| ParseError("workbook has no sheets".into()))?
    };
    let range = wb
        .worksheet_range(&sheet)
        .map_err(|e| ParseError(format!("cannot read sheet {sheet:?}: {e}")))?;

    let mut rows_iter = range.rows();
    let header = rows_iter
        .next()
        .ok_or_else(|| ParseError("sheet is empty (no header row)".into()))?;

    // Map documented column name → index. Fail closed if any required column is absent.
    let header_text: Vec<String> = header.iter().map(cell_text).collect();
    let mut idx: HashMap<&str, usize> = HashMap::new();
    for col in COLS {
        let pos = header_text
            .iter()
            .position(|h| h == col)
            .ok_or_else(|| ParseError(format!("missing required column: {col:?}")))?;
        idx.insert(col, pos);
    }
    let get = |row: &[Data], col: &str| -> String {
        idx.get(col)
            .and_then(|&i| row.get(i))
            .map(cell_text)
            .unwrap_or_default()
    };
    let get_bool = |row: &[Data], col: &str| -> bool {
        idx.get(col)
            .and_then(|&i| row.get(i))
            .map(cell_bool)
            .unwrap_or(false)
    };

    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Group> = HashMap::new();
    let mut data_rows = 0usize;

    for row in rows_iter {
        let reference = get(row, "Reference");
        let name = get(row, "Name of Individual or Entity");
        if reference.is_empty() && name.is_empty() {
            continue; // skip blank trailing rows
        }
        data_rows += 1;
        if data_rows > MAX_DATA_ROWS {
            return Err(ParseError(format!(
                "row cap exceeded: > {MAX_DATA_ROWS} data rows"
            )));
        }

        let key = group_key(&reference);
        let name_type = get(row, "Name Type");
        let alias_strength = get(row, "Alias Strength");
        let is_primary = name_type.to_lowercase().contains("primary");
        let is_original_script = name_type.to_lowercase().contains("original script");

        let g = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key.clone());
            Group {
                r#ref: key.clone(),
                ..Default::default()
            }
        });

        g.names.push(NameEntry {
            name: name.clone(),
            name_type: name_type.clone(),
            alias_strength: if alias_strength.is_empty() {
                None
            } else {
                Some(alias_strength)
            },
            script: is_original_script,
        });

        // Biodata + flags come from the primary-name row of the group.
        if is_primary {
            g.has_primary = true;
            g.primary_name = name.clone();
            g.entry_type = map_entry_type(&get(row, "Type"));
            g.dates_of_birth = split_commas(&get(row, "Date of Birth"));
            g.places_of_birth = one_if_present(&get(row, "Place of Birth"));
            g.citizenships = split_commas(&get(row, "Citizenship"));
            g.addresses = one_if_present(&get(row, "Address"));
            g.additional_info = get(row, "Additional Information");
            g.listing_info = get(row, "Listing Information");
            g.imo_number = get(row, "IMO Number");
            g.committees = get(row, "Committees");
            g.control_date = get(row, "Control Date");
            g.instrument_of_designation = get(row, "Instrument of Designation");
            g.targeted_financial_sanction = get_bool(row, "Targeted Financial Sanction");
            g.travel_ban = get_bool(row, "Travel Ban");
            g.arms_embargo = get_bool(row, "Arms Embargo");
            g.maritime_restriction = get_bool(row, "Maritime Restriction");
        } else if g.entry_type.is_empty() {
            // First-seen non-primary row still establishes a fallback type.
            g.entry_type = map_entry_type(&get(row, "Type"));
        }
    }

    // Materialise records in first-seen order.
    let mut out = Vec::with_capacity(order.len());
    for key in order {
        let mut g = groups.remove(&key).expect("group present");
        if g.primary_name.is_empty() {
            // No row was flagged Primary Name — fall back to the first name.
            g.primary_name = g.names.first().map(|n| n.name.clone()).unwrap_or_default();
        }
        let all_names = dedup_nonempty(g.names.iter().map(|n| n.name.clone()));
        let all_names_norm = dedup_nonempty(all_names.iter().map(|n| normalize_for_search(n)));
        let all_names_phonetic = dedup_nonempty(
            all_names
                .iter()
                .flat_map(|n| phonetic_tokens(n))
                .collect::<Vec<_>>(),
        );

        out.push(DesignatedPerson {
            r#ref: g.r#ref,
            entry_type: g.entry_type,
            primary_name: g.primary_name,
            all_names,
            all_names_norm,
            all_names_phonetic,
            names: g.names,
            dates_of_birth: g.dates_of_birth,
            places_of_birth: g.places_of_birth,
            citizenships: g.citizenships,
            addresses: g.addresses,
            additional_info: g.additional_info,
            listing_info: g.listing_info,
            imo_number: g.imo_number,
            committees: g.committees,
            control_date: g.control_date,
            instrument_of_designation: g.instrument_of_designation,
            targeted_financial_sanction: g.targeted_financial_sanction,
            travel_ban: g.travel_ban,
            arms_embargo: g.arms_embargo,
            maritime_restriction: g.maritime_restriction,
        });
    }

    Ok((out, data_rows))
}
