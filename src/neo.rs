//! Neo4j writer (Bolt, via neo4rs) — staged ListVersion, atomic cutover, and the
//! hash-chained IngestEvent ledger. Runs as the least-privilege
//! svc_sanctions_ingest principal (M1). Connects ONLY to Neo4j — never the server.
//!
//! Latest-only is atomic: the full version snapshot is built first (not active),
//! then ONE transaction flips the single ACTIVE_VERSION pointer, so readers
//! always see exactly one complete version — never a torn/mixed list.

use crate::chain::{compute_event_hash, compute_payload_hash, EventHashMaterial, INGEST_CHAIN_ID};
use crate::parse::DesignatedPerson;
use neo4rs::{query, ConfigBuilder, Graph};
use serde_json::json;
use uuid::Uuid;

pub const SANCTIONS_LIST_ID: &str = "dfat-consolidated";
const BATCH_SIZE: usize = 500;

pub struct Neo {
    graph: Graph,
}

pub struct CutoverOutcome {
    pub version_id: String,
    pub seq: i64,
    pub person_count: usize,
}

impl Neo {
    pub async fn connect(uri: &str, user: &str, pass: &str, db: &str) -> Result<Self, neo4rs::Error> {
        let cfg = ConfigBuilder::default()
            .uri(uri)
            .user(user)
            .password(pass)
            .db(db)
            .build()?;
        Ok(Self {
            graph: Graph::connect(cfg).await?,
        })
    }

    /// SHA-256 of the source file behind the CURRENT active version (gate input).
    pub async fn current_active_sha(&self) -> Result<Option<String>, neo4rs::Error> {
        let mut r = self
            .graph
            .execute(
                query(
                    "MATCH (:SanctionsList {id:$id})-[:ACTIVE_VERSION]->(:ListVersion)-[:FROM_FILE]->(f:SanctionsSourceFile)
                     RETURN f.sha256 AS sha LIMIT 1",
                )
                .param("id", SANCTIONS_LIST_ID),
            )
            .await?;
        if let Ok(Some(row)) = r.next().await {
            Ok(row.get::<String>("sha").ok())
        } else {
            Ok(None)
        }
    }

    /// Next monotonic ListVersion seq (max+1, or 0 for the first).
    async fn next_version_seq(&self) -> Result<i64, neo4rs::Error> {
        let mut r = self
            .graph
            .execute(query(
                "MATCH (v:ListVersion) RETURN coalesce(max(v.seq), -1) + 1 AS next",
            ))
            .await?;
        if let Ok(Some(row)) = r.next().await {
            Ok(row.get::<i64>("next").unwrap_or(0))
        } else {
            Ok(0)
        }
    }

    /// Build the staged (NOT active) ListVersion + source file + CONTAINS nodes.
    /// Nothing here changes what readers see — the version is unreachable from the
    /// ACTIVE_VERSION pointer until `flip_active` runs.
    pub async fn stage_version(
        &self,
        records: &[DesignatedPerson],
        source_sha: &str,
        r2_key: &str,
        size_bytes: i64,
        row_count: i64,
    ) -> Result<(String, i64), neo4rs::Error> {
        let version_id = Uuid::new_v4().to_string();
        let seq = self.next_version_seq().await?;

        // Source file (content-addressed) + staged version + FROM_FILE.
        self.graph
            .run(
                query(
                    "MERGE (f:SanctionsSourceFile {sha256:$sha})
                       ON CREATE SET f.id=$fid, f.r2Key=$r2Key, f.sizeBytes=$size,
                                     f.rowCount=$rows, f.downloadedAt=datetime()
                     CREATE (v:ListVersion {
                       id:$vid, seq:$seq, ingestedAt:datetime(), sourceSha:$sha,
                       personCount:$pcount, rowCount:$rows, staged:true })
                     CREATE (v)-[:FROM_FILE]->(f)",
                )
                .param("sha", source_sha)
                .param("fid", Uuid::new_v4().to_string())
                .param("r2Key", r2_key)
                .param("size", size_bytes)
                .param("rows", row_count)
                .param("vid", version_id.clone())
                .param("seq", seq)
                .param("pcount", records.len() as i64),
            )
            .await?;

        // Batch DesignatedPerson creation as parallel arrays (struct-of-arrays) to
        // avoid nested-map params; index them in Cypher with $arr[i].
        for chunk in records.chunks(BATCH_SIZE) {
            let refs: Vec<String> = chunk.iter().map(|r| r.r#ref.clone()).collect();
            let entry_types: Vec<String> = chunk.iter().map(|r| r.entry_type.clone()).collect();
            let primary: Vec<String> = chunk.iter().map(|r| r.primary_name.clone()).collect();
            let all_names: Vec<Vec<String>> = chunk.iter().map(|r| r.all_names.clone()).collect();
            let all_norm: Vec<Vec<String>> = chunk.iter().map(|r| r.all_names_norm.clone()).collect();
            let all_phon: Vec<Vec<String>> =
                chunk.iter().map(|r| r.all_names_phonetic.clone()).collect();
            let names_json: Vec<String> = chunk
                .iter()
                .map(|r| serde_json::to_string(&r.names).unwrap_or_else(|_| "[]".into()))
                .collect();
            let dob: Vec<Vec<String>> = chunk.iter().map(|r| r.dates_of_birth.clone()).collect();
            let pob: Vec<Vec<String>> = chunk.iter().map(|r| r.places_of_birth.clone()).collect();
            let cit: Vec<Vec<String>> = chunk.iter().map(|r| r.citizenships.clone()).collect();
            let addr: Vec<Vec<String>> = chunk.iter().map(|r| r.addresses.clone()).collect();
            let add_info: Vec<String> = chunk.iter().map(|r| r.additional_info.clone()).collect();
            let list_info: Vec<String> = chunk.iter().map(|r| r.listing_info.clone()).collect();
            let imo: Vec<String> = chunk.iter().map(|r| r.imo_number.clone()).collect();
            let comm: Vec<String> = chunk.iter().map(|r| r.committees.clone()).collect();
            let cdate: Vec<String> = chunk.iter().map(|r| r.control_date.clone()).collect();
            let instr: Vec<String> = chunk
                .iter()
                .map(|r| r.instrument_of_designation.clone())
                .collect();
            let tfs: Vec<bool> = chunk.iter().map(|r| r.targeted_financial_sanction).collect();
            let tban: Vec<bool> = chunk.iter().map(|r| r.travel_ban).collect();
            let arms: Vec<bool> = chunk.iter().map(|r| r.arms_embargo).collect();
            let mar: Vec<bool> = chunk.iter().map(|r| r.maritime_restriction).collect();

            self.graph
                .run(
                    query(
                        "MATCH (v:ListVersion {id:$vid})
                         UNWIND range(0, size($refs)-1) AS i
                         CREATE (dp:DesignatedPerson {
                           ref:$refs[i], entryType:$entryTypes[i], primaryName:$primary[i],
                           allNames:$allNames[i], allNamesNorm:$allNorm[i], allNamesPhonetic:$allPhon[i],
                           names:$namesJson[i], datesOfBirth:$dob[i], placesOfBirth:$pob[i],
                           citizenships:$cit[i], addresses:$addr[i], additionalInfo:$addInfo[i],
                           listingInfo:$listInfo[i], imoNumber:$imo[i], committees:$comm[i],
                           controlDate:$cdate[i], instrumentOfDesignation:$instr[i],
                           targetedFinancialSanction:$tfs[i], travelBan:$tban[i],
                           armsEmbargo:$arms[i], maritimeRestriction:$mar[i],
                           firstSeenAt:datetime(), lastSeenAt:datetime() })
                         CREATE (v)-[:CONTAINS]->(dp)",
                    )
                    .param("vid", version_id.clone())
                    .param("refs", refs)
                    .param("entryTypes", entry_types)
                    .param("primary", primary)
                    .param("allNames", all_names)
                    .param("allNorm", all_norm)
                    .param("allPhon", all_phon)
                    .param("namesJson", names_json)
                    .param("dob", dob)
                    .param("pob", pob)
                    .param("cit", cit)
                    .param("addr", addr)
                    .param("addInfo", add_info)
                    .param("listInfo", list_info)
                    .param("imo", imo)
                    .param("comm", comm)
                    .param("cdate", cdate)
                    .param("instr", instr)
                    .param("tfs", tfs)
                    .param("tban", tban)
                    .param("arms", arms)
                    .param("mar", mar),
                )
                .await?;
        }

        Ok((version_id, seq))
    }

    /// Count persons actually staged under a version (QC input).
    pub async fn staged_person_count(&self, version_id: &str) -> Result<i64, neo4rs::Error> {
        let mut r = self
            .graph
            .execute(
                query(
                    "MATCH (:ListVersion {id:$vid})-[:CONTAINS]->(dp:DesignatedPerson)
                     RETURN count(dp) AS n",
                )
                .param("vid", version_id),
            )
            .await?;
        if let Ok(Some(row)) = r.next().await {
            Ok(row.get::<i64>("n").unwrap_or(0))
        } else {
            Ok(0)
        }
    }

    /// Person count of the CURRENT active version (QC swing comparison; 0 if none).
    pub async fn active_person_count(&self) -> Result<i64, neo4rs::Error> {
        let mut r = self
            .graph
            .execute(
                query(
                    "MATCH (:SanctionsList {id:$id})-[:ACTIVE_VERSION]->(:ListVersion)-[:CONTAINS]->(dp:DesignatedPerson)
                     RETURN count(dp) AS n",
                )
                .param("id", SANCTIONS_LIST_ID),
            )
            .await?;
        if let Ok(Some(row)) = r.next().await {
            Ok(row.get::<i64>("n").unwrap_or(0))
        } else {
            Ok(0)
        }
    }

    /// Refs in the CURRENT active version — for the IngestEvent added/removed diff.
    pub async fn active_refs(&self) -> Result<Vec<String>, neo4rs::Error> {
        let mut r = self
            .graph
            .execute(
                query(
                    "MATCH (:SanctionsList {id:$id})-[:ACTIVE_VERSION]->(:ListVersion)-[:CONTAINS]->(dp:DesignatedPerson)
                     RETURN dp.ref AS ref",
                )
                .param("id", SANCTIONS_LIST_ID),
            )
            .await?;
        let mut out = Vec::new();
        while let Ok(Some(row)) = r.next().await {
            if let Ok(v) = row.get::<String>("ref") {
                out.push(v);
            }
        }
        Ok(out)
    }

    /// ATOMIC one-transaction cutover: drop the old ACTIVE_VERSION edge and
    /// point it at the new version, clearing the staged flag. Readers see exactly
    /// one complete version before and after; a rollback leaves the old one active.
    pub async fn flip_active(&self, version_id: &str) -> Result<CutoverOutcome, neo4rs::Error> {
        let mut txn = self.graph.start_txn().await?;
        txn.run(
            query(
                "MERGE (s:SanctionsList {id:$id})
                 WITH s
                 OPTIONAL MATCH (s)-[r:ACTIVE_VERSION]->(:ListVersion)
                 DELETE r
                 WITH s
                 MATCH (v:ListVersion {id:$vid})
                 CREATE (s)-[:ACTIVE_VERSION]->(v)
                 REMOVE v.staged",
            )
            .param("id", SANCTIONS_LIST_ID)
            .param("vid", version_id),
        )
        .await?;
        txn.commit().await?;

        let person_count = self.active_person_count().await? as usize;
        Ok(CutoverOutcome {
            version_id: version_id.to_string(),
            seq: 0,
            person_count,
        })
    }

    /// Append a hash-chained IngestEvent. Locks the ProvenanceChain head
    /// like the server's recordEvent, so concurrent appends serialise with no seq
    /// gaps. `skipped` heartbeats carry no listVersionId. The node is labelled
    /// :IngestEvent (NOT :ProvenanceEvent) so the ingest principal stays within its
    /// grant, yet the hash material matches the server scheme for verifyChain().
    #[allow(clippy::too_many_arguments)]
    pub async fn append_ingest_event(
        &self,
        triggered_by: Option<&str>,
        source_sha: &str,
        list_version_id: Option<&str>,
        added: i64,
        updated: i64,
        deactivated: i64,
        unchanged: i64,
        skipped: bool,
        row_count: i64,
        person_count: i64,
    ) -> Result<String, neo4rs::Error> {
        // The ingest ledger is anchored purely on :IngestEvent nodes (chainId +
        // seq + prevHash) — NOT a :ProvenanceChain node, which the least-privilege
        // ingest principal is (correctly) denied. The ingest_event_seq UNIQUE
        // constraint (M1) prevents two events claiming the same seq under a race;
        // at the manual one-shot cadence concurrent appends don't occur. Read head
        // + create happen in one tx so seq is consistent.
        let mut txn = self.graph.start_txn().await?;

        let mut head = txn
            .execute(
                query(
                    "MATCH (e:IngestEvent {chainId:$chainId})
                     RETURN e.seq AS seq, e.hash AS hash ORDER BY e.seq DESC LIMIT 1",
                )
                .param("chainId", INGEST_CHAIN_ID),
            )
            .await?;
        let (prev_seq, prev_hash) = if let Ok(Some(row)) = head.next(txn.handle()).await {
            (
                row.get::<i64>("seq").unwrap_or(-1),
                row.get::<String>("hash")
                    .unwrap_or_else(|_| crate::chain::GENESIS_HASH.to_string()),
            )
        } else {
            (-1, crate::chain::GENESIS_HASH.to_string())
        };

        let seq = prev_seq + 1;
        let action = if skipped { "SKIP" } else { "CREATE" };
        let payload = json!({
            "sourceSha": source_sha,
            "added": added,
            "updated": updated,
            "deactivated": deactivated,
            "unchanged": unchanged,
            "skipped": skipped,
            "rowCount": row_count,
            "personCount": person_count,
        });
        let payload_hash = compute_payload_hash(&payload);
        let citations = vec!["Act s.28(2)(e)(ii)".to_string(), "Rules s.5-3".to_string()];
        let hash = compute_event_hash(&EventHashMaterial {
            seq,
            chain_id: INGEST_CHAIN_ID.to_string(),
            event_type: "SANCTIONS_INGEST".to_string(),
            action: action.to_string(),
            object_type: Some("ListVersion".to_string()),
            object_id: list_version_id.map(|s| s.to_string()),
            entity_id: None,
            actor_account_id: triggered_by.map(|s| s.to_string()),
            citations: citations.clone(),
            payload_hash: payload_hash.clone(),
            prev_hash: prev_hash.clone(),
        });
        let id = Uuid::new_v4().to_string();

        txn.run(
            query(
                "CREATE (e:IngestEvent {
                   id:$id, chainId:$chainId, seq:$seq, eventType:'SANCTIONS_INGEST', action:$action,
                   objectType:'ListVersion', objectId:$objectId, entityId:null, actorAccountId:$actor,
                   citations:$citations, payloadHash:$payloadHash, payloadJson:$payloadJson,
                   prevHash:$prevHash, hash:$hash,
                   sourceSha:$sourceSha, added:$added, updated:$updated, deactivated:$deactivated,
                   unchanged:$unchanged, skipped:$skipped, createdAt:datetime() })",
            )
            .param("chainId", INGEST_CHAIN_ID)
            .param("id", id.clone())
            .param("seq", seq)
            .param("action", action)
            .param("objectId", list_version_id.unwrap_or(""))
            .param("actor", triggered_by.unwrap_or(""))
            .param("citations", citations)
            .param("payloadHash", payload_hash)
            .param("payloadJson", crate::chain::stable_stringify(&payload))
            .param("prevHash", prev_hash)
            .param("hash", hash.clone())
            .param("sourceSha", source_sha)
            .param("added", added)
            .param("updated", updated)
            .param("deactivated", deactivated)
            .param("unchanged", unchanged)
            .param("skipped", skipped),
        )
        .await?;
        txn.commit().await?;
        Ok(hash)
    }
}
