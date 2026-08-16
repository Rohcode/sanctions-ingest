//! Hash-chained IngestEvent ledger — byte-compatible with the server's
//! recordEvent.ts scheme. The container computes IngestEvent hashes
//! with the SAME canonical-JSON + SHA-256 + ordered-array + prevHash chaining as
//! the server, so the server's existing verifyChain() validates the ingest chain
//! by reading :IngestEvent nodes — WITHOUT the container ever connecting to the
//! server. Parity is proven by tests/hash_parity.rs against vectors generated
//! from the TS source.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Hash of the (non-existent) event before the first one in any chain.
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// The fixed chain id for the DFAT ingest ledger.
pub const INGEST_CHAIN_ID: &str = "sanctions-ingest-dfat";

pub fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Deterministic JSON — port of recordEvent.ts `stableStringify`: object keys
/// sorted recursively, arrays preserved, scalars via JSON encoding.
pub fn stable_stringify(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(_) => serde_json::to_string(value).expect("string encodes"),
        Value::Array(a) => {
            let inner: Vec<String> = a.iter().map(stable_stringify).collect();
            format!("[{}]", inner.join(","))
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    let key = serde_json::to_string(k).expect("key encodes");
                    format!("{}:{}", key, stable_stringify(&map[k]))
                })
                .collect();
            format!("{{{}}}", inner.join(","))
        }
    }
}

/// Port of `computePayloadHash`: sha256(stableStringify(payload ?? {})).
pub fn compute_payload_hash(payload: &Value) -> String {
    let effective = if payload.is_null() { json!({}) } else { payload.clone() };
    sha256_hex(&stable_stringify(&effective))
}

/// Fields committed to by an event's hash (createdAt excluded — DB metadata).
#[derive(Clone, Debug)]
pub struct EventHashMaterial {
    pub seq: i64,
    pub chain_id: String,
    pub event_type: String,
    pub action: String,
    pub object_type: Option<String>,
    pub object_id: Option<String>,
    pub entity_id: Option<String>,
    pub actor_account_id: Option<String>,
    pub citations: Vec<String>,
    pub payload_hash: String,
    pub prev_hash: String,
}

/// Port of `computeEventHash`: sha256 of the ordered material array, with
/// null→'' coercion and sorted citations, exactly as the server does.
pub fn compute_event_hash(m: &EventHashMaterial) -> String {
    let mut citations = m.citations.clone();
    citations.sort();
    let arr = json!([
        m.seq,
        m.chain_id,
        m.event_type,
        m.action,
        m.object_type.clone().unwrap_or_default(),
        m.object_id.clone().unwrap_or_default(),
        m.entity_id.clone().unwrap_or_default(),
        m.actor_account_id.clone().unwrap_or_default(),
        citations,
        m.payload_hash,
        m.prev_hash,
    ]);
    sha256_hex(&stable_stringify(&arr))
}

/// A stored IngestEvent, as read back for verification.
#[derive(Clone, Debug)]
pub struct IngestEventRecord {
    pub seq: i64,
    pub chain_id: String,
    pub event_type: String,
    pub action: String,
    pub object_type: Option<String>,
    pub object_id: Option<String>,
    pub entity_id: Option<String>,
    pub actor_account_id: Option<String>,
    pub citations: Vec<String>,
    pub payload_hash: String,
    pub prev_hash: String,
    pub hash: String,
}

#[derive(Debug)]
pub struct ChainVerification {
    pub valid: bool,
    pub event_count: usize,
    pub broken_at_seq: Option<i64>,
    pub message: String,
}

/// Port of `verifyChain`: contiguous seq from 0, prevHash links, recomputed
/// hashes. Events may arrive unordered; sorted by seq internally.
pub fn verify_chain(events: &[IngestEventRecord]) -> ChainVerification {
    let mut ordered = events.to_vec();
    ordered.sort_by_key(|e| e.seq);
    let event_count = ordered.len();

    if event_count == 0 {
        return ChainVerification {
            valid: false,
            event_count: 0,
            broken_at_seq: None,
            message: "No events found for this chain.".into(),
        };
    }

    let mut prev_hash = GENESIS_HASH.to_string();
    for (i, e) in ordered.iter().enumerate() {
        if e.seq != i as i64 {
            return ChainVerification {
                valid: false,
                event_count,
                broken_at_seq: Some(e.seq),
                message: format!("Sequence gap at position {i}: event reports seq {}.", e.seq),
            };
        }
        if e.prev_hash != prev_hash {
            return ChainVerification {
                valid: false,
                event_count,
                broken_at_seq: Some(e.seq),
                message: format!("Broken link at seq {}: prevHash mismatch.", e.seq),
            };
        }
        let recomputed = compute_event_hash(&EventHashMaterial {
            seq: e.seq,
            chain_id: e.chain_id.clone(),
            event_type: e.event_type.clone(),
            action: e.action.clone(),
            object_type: e.object_type.clone(),
            object_id: e.object_id.clone(),
            entity_id: e.entity_id.clone(),
            actor_account_id: e.actor_account_id.clone(),
            citations: e.citations.clone(),
            payload_hash: e.payload_hash.clone(),
            prev_hash: e.prev_hash.clone(),
        });
        if recomputed != e.hash {
            return ChainVerification {
                valid: false,
                event_count,
                broken_at_seq: Some(e.seq),
                message: format!("Tampering detected at seq {}: stored hash != recomputed.", e.seq),
            };
        }
        prev_hash = e.hash.clone();
    }

    ChainVerification {
        valid: true,
        event_count,
        broken_at_seq: None,
        message: format!("Verified: {event_count} events form an unbroken chain."),
    }
}
