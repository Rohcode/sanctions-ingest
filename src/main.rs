//! sanctions-ingest CLI — one-shot, manual-trigger DFAT Consolidated List ingest.
//!
//! Usage:
//!   sanctions-ingest --file <path.xlsx> [--out <records.json>]     # parse only
//!   sanctions-ingest --file <path.xlsx> --commit [--triggered-by <id>] [--skip-archive]
//!
//! --commit runs the full pipeline: SHA-256 gate → R2 archive → staged
//! ListVersion → QC gate → atomic ACTIVE_VERSION flip → hash-chained IngestEvent.
//! It reads connection secrets from env (NEVER baked into the image):
//!   NEO4J_URI, NEO4J_USERNAME (svc_sanctions_ingest), NEO4J_PASSWORD, NEO4J_DATABASE
//!   R2_ENDPOINT, R2_ACCESS_KEY_ID, R2_SECRET_ACCESS_KEY, R2_BUCKET
//!
//! Fail-closed: QC halts BEFORE the flip on an empty or wildly-swinging list, so a
//! truncated download can never become the active list.

use sanctions_ingest::neo::Neo;
use sanctions_ingest::parse::{parse_consolidated_list, DesignatedPerson, MAX_BYTES};
use sanctions_ingest::r2::R2;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::ExitCode;

const QC_SWING: f64 = 0.20; // ±20% person-count swing halts before cutover

fn usage() -> ExitCode {
    eprintln!(
        "usage: sanctions-ingest --file <path.xlsx> [--out <records.json>] \
         [--commit [--triggered-by <id>] [--skip-archive]]"
    );
    ExitCode::from(2)
}

fn env(key: &str) -> Result<String, String> {
    std::env::var(key).map_err(|_| format!("missing required env var: {key}"))
}

#[tokio::main]
async fn main() -> ExitCode {
    let mut file: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut commit = false;
    let mut skip_archive = false;
    let mut triggered_by: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--file" => file = args.next().map(PathBuf::from),
            "--out" => out = args.next().map(PathBuf::from),
            "--commit" => commit = true,
            "--skip-archive" => skip_archive = true,
            "--triggered-by" => triggered_by = args.next(),
            "-h" | "--help" => return usage(),
            other => {
                eprintln!("unexpected argument: {other}");
                return usage();
            }
        }
    }
    let Some(file) = file else { return usage() };

    // Pre-parse byte cap (fail closed before reading into the parser).
    let meta = match std::fs::metadata(&file) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("✗ cannot stat {}: {e}", file.display());
            return ExitCode::FAILURE;
        }
    };
    if meta.len() > MAX_BYTES {
        eprintln!("✗ file too large: {} bytes > cap {MAX_BYTES} — rejected", meta.len());
        return ExitCode::FAILURE;
    }
    let bytes = match std::fs::read(&file) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("✗ cannot read {}: {e}", file.display());
            return ExitCode::FAILURE;
        }
    };
    let sha256 = hex(&Sha256::digest(&bytes));

    let (records, data_rows) = match parse_consolidated_list(&file) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("✗ parse failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    if let Some(out) = &out {
        match serde_json::to_vec_pretty(&records).map_err(|e| e.to_string()).and_then(|j| {
            std::fs::write(out, j).map_err(|e| e.to_string())
        }) {
            Ok(_) => eprintln!("· wrote {} records → {}", records.len(), out.display()),
            Err(e) => {
                eprintln!("✗ cannot write records: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    if !commit {
        let summary = serde_json::json!({
            "sourceFileSha": sha256, "sizeBytes": meta.len(),
            "rowCount": data_rows, "personCount": records.len(),
        });
        println!("{}", serde_json::to_string_pretty(&summary).unwrap());
        return ExitCode::SUCCESS;
    }

    match commit_pipeline(&records, &sha256, meta.len() as i64, data_rows as i64, skip_archive, triggered_by.as_deref(), bytes).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("✗ ingest failed: {e}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn commit_pipeline(
    records: &[DesignatedPerson],
    sha256: &str,
    size_bytes: i64,
    data_rows: i64,
    skip_archive: bool,
    triggered_by: Option<&str>,
    bytes: Vec<u8>,
) -> Result<ExitCode, String> {
    let neo = Neo::connect(
        &env("NEO4J_URI")?,
        &env("NEO4J_USERNAME")?,
        &env("NEO4J_PASSWORD")?,
        &env("NEO4J_DATABASE")?,
    )
    .await
    .map_err(|e| format!("Neo4j connect: {e}"))?;

    // 1. SHA-256 gate — identical file ⇒ heartbeat skip event, stop.
    let current = neo.current_active_sha().await.map_err(|e| e.to_string())?;
    if current.as_deref() == Some(sha256) {
        neo.append_ingest_event(triggered_by, sha256, None, 0, 0, 0, 0, true, data_rows, records.len() as i64)
            .await
            .map_err(|e| e.to_string())?;
        println!("{}", serde_json::json!({"skipped": true, "sourceFileSha": sha256, "reason": "unchanged"}));
        return Ok(ExitCode::SUCCESS);
    }

    // 2. R2 content-addressed archive BEFORE touching the graph (abort if it fails).
    let r2_key = format!("sanctions/dfat/{sha256}.xlsx");
    if !skip_archive {
        let r2 = R2::new(&env("R2_ENDPOINT")?, &env("R2_ACCESS_KEY_ID")?, &env("R2_SECRET_ACCESS_KEY")?, &env("R2_BUCKET")?);
        let uploaded = r2
            .archive_if_absent(&r2_key, bytes, "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
            .await?;
        eprintln!("· R2 archive {} ({})", r2_key, if uploaded { "uploaded" } else { "already present" });
    }

    // 3. Build the staged (not-active) version.
    let (version_id, seq) = neo.stage_version(records, sha256, &r2_key, size_bytes, data_rows).await.map_err(|e| e.to_string())?;

    // 4. QC gate — fail closed BEFORE the flip.
    let staged = neo.staged_person_count(&version_id).await.map_err(|e| e.to_string())?;
    if staged == 0 {
        return Err(format!("QC halt: staged version {version_id} has 0 persons — refusing to flip (fail closed)"));
    }
    if staged as usize != records.len() {
        return Err(format!("QC halt: staged {staged} != parsed {} — partial write, refusing to flip", records.len()));
    }
    let prev_refs: HashSet<String> = neo.active_refs().await.map_err(|e| e.to_string())?.into_iter().collect();
    if !prev_refs.is_empty() {
        let swing = (staged - prev_refs.len() as i64).abs() as f64 / prev_refs.len() as f64;
        if swing > QC_SWING {
            return Err(format!(
                "QC halt: person count swing {:.1}% (>{:.0}%) from {} to {} — manual review required",
                swing * 100.0, QC_SWING * 100.0, prev_refs.len(), staged
            ));
        }
    }

    // diff vs previous active version (snapshot model: by ref membership).
    let new_refs: HashSet<&str> = records.iter().map(|r| r.r#ref.as_str()).collect();
    let added = new_refs.iter().filter(|r| !prev_refs.contains(**r)).count() as i64;
    let deactivated = prev_refs.iter().filter(|r| !new_refs.contains(r.as_str())).count() as i64;
    let unchanged = new_refs.iter().filter(|r| prev_refs.contains(**r)).count() as i64;

    // 5. ATOMIC cutover.
    let outcome = neo.flip_active(&version_id).await.map_err(|e| e.to_string())?;

    // 6. Hash-chained IngestEvent (written last; commits the version + sha).
    let event_hash = neo
        .append_ingest_event(triggered_by, sha256, Some(&version_id), added, 0, deactivated, unchanged, false, data_rows, outcome.person_count as i64)
        .await
        .map_err(|e| e.to_string())?;

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "skipped": false,
            "versionId": version_id,
            "seq": seq,
            "sourceFileSha": sha256,
            "personCount": outcome.person_count,
            "added": added,
            "deactivated": deactivated,
            "unchanged": unchanged,
            "ingestEventHash": event_hash,
        }))
        .unwrap()
    );
    Ok(ExitCode::SUCCESS)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
