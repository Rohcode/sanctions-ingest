# sanctions-ingest

[![CI](https://github.com/Rohcode/sanctions-ingest/actions/workflows/ci.yml/badge.svg)](https://github.com/Rohcode/sanctions-ingest/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.83-orange.svg)](https://www.rust-lang.org/)
[![Container](https://img.shields.io/badge/container-distroless%20%7C%20nonroot%20%7C%20no--network-informational)](#hardened-container-run)

**A transactional ingest pipeline for Australia's sanctions list — written in
Rust, designed to fail closed.**

Banks and regulated businesses are required to check customers against government
sanctions lists. This is the component that takes the Australian government's
published list — an XLSX file downloaded from a website — and turns it into the
searchable data that screening runs against.

The interesting constraint is the failure mode. If this step goes wrong, it
usually doesn't throw an error: it quietly stops matching a name that should have
matched, and nothing downstream notices. A stale-but-correct list is much safer
than a fresh-but-wrong one, so every checkpoint here halts rather than degrades.

That trade-off drives most of the design decisions below.

*(DFAT — the Department of Foreign Affairs and Trade — is the Australian
government department that publishes the list.)*

---

## Contents

- [What it actually does](#what-it-actually-does) · [Pipeline](#the-pipeline)
- [Why Rust](#why-rust-specifically) · [Why a separate binary](#why-this-is-a-separate-binary)
- [Where it halts](#where-it-halts) · [The parity test](#the-parity-test)
- [Quickstart](#quickstart) · [Hardened container](#hardened-container-run)
- [Supply chain](#supply-chain-controls) · [Repo map](#repo-map) · [Status](#status)

---

## What it actually does

Two modes. Parse-only is a pure function of the input file. `--commit` runs the
full transactional pipeline against live infrastructure.

```bash
sanctions-ingest --file dfat.xlsx --out records.json   # parse only, no side effects
sanctions-ingest --file dfat.xlsx --commit             # full versioned cutover
```

**Parsing**
- Reads the XLSX with `calamine`, **every cell as text** — no formula evaluation.
- Maps the full documented 20-column schema from *Guide to Australia's Consolidated List*.
- Groups rows by **suffix-stripped Reference** (`1000a → 1000`), producing one
  record per listing with aliases and original-script names folded in.
- Derives normalised and phonetic name forms for matching, held to the platform's
  TypeScript normaliser by a golden-vector test ([see below](#the-parity-test)).
- Emits per-listing sanction flags: targeted financial sanction, travel ban,
  arms embargo, maritime restriction.

**Committing** (`--commit`)
- **SHA-256 gate** — an unchanged file is a no-op heartbeat, not a re-ingest.
- **Content-addressed archive** to object storage *before* the graph is touched.
- **Staged version** built offline, invisible to searches until it passes QC.
- **QC gate** — halts before cutover on an empty list, a partial write, or a
  person-count swing over ±20%.
- **Atomic cutover** — searches see the old list or the new one, never a mix.
- **Hash-chained ingest event** — an append-only, tamper-evident audit ledger.

> For scale: a published snapshot of the list is roughly **11,000 rows**, which
> groups down to about **3,800 designated persons** plus entities and vessels.

---

## The pipeline

```
        ┌─────────────────────────────────────────────────────────────┐
        │  UNTRUSTED INPUT                                            │
        │  DFAT Consolidated List (.xlsx), operator-downloaded        │
        └───────────────────────────┬─────────────────────────────────┘
                                    │
   ╔════════════════════════════════▼═════════════════════════════════╗
   ║  sanctions-ingest    no network · read-only fs · caps dropped    ║
   ║  ┌────────────────────────────────────────────────────────────┐  ║
   ║  │ ① PRE-PARSE CAPS      ≤ 8 MB · ≤ 20,000 rows      ✗ reject │  ║
   ║  ├────────────────────────────────────────────────────────────┤  ║
   ║  │ ② PARSE               every cell as text, no formulas      │  ║
   ║  │                       20-column schema → grouped listings  │  ║
   ║  ├────────────────────────────────────────────────────────────┤  ║
   ║  │ ③ NORMALISE           fold case · strip marks · phonetic   │  ║
   ║  │                       ⚖ parity-locked to the TS normaliser │  ║
   ║  └────────────────────────────────────────────────────────────┘  ║
   ╚════════════════════════════════╤═════════════════════════════════╝
                                    │        ── --commit only ──
              ┌─────────────────────▼──────────────────────┐
              │ ④ SHA-256 GATE   unchanged file? ──────────┼──▶ heartbeat, stop
              └─────────────────────┬──────────────────────┘
                                    │
              ┌─────────────────────▼──────────────────────┐
              │ ⑤ ARCHIVE  →  object storage, write-only   │
              │    key = sanctions/dfat/<sha256>.xlsx      │
              │    fails? abort before touching the graph  │
              └─────────────────────┬──────────────────────┘
                                    │
              ┌─────────────────────▼──────────────────────┐
              │ ⑥ STAGE    build version, NOT active yet   │
              └─────────────────────┬──────────────────────┘
                                    │
              ┌─────────────────────▼──────────────────────┐
              │ ⑦ QC GATE                       fail closed│
              │    empty list        → HALT                │
              │    staged ≠ parsed   → HALT (partial write)│
              │    ±20% count swing  → HALT (manual review)│
              └─────────────────────┬──────────────────────┘
                                    │ all clear
              ┌─────────────────────▼──────────────────────┐
              │ ⑧ ATOMIC FLIP   ACTIVE_VERSION → new       │
              │    searches never observe a mixed list     │
              └─────────────────────┬──────────────────────┘
                                    │
              ┌─────────────────────▼──────────────────────┐
              │ ⑨ LEDGER   hash-chained IngestEvent        │
              │    prevHash ← … ← genesis   tamper-evident │
              └─────────────────────┬──────────────────────┘
                                    │
   ┌────────────────────────────────▼─────────────────────────────────┐
   │  PLATFORM SERVER — strictly QUERY-ONLY, holds no write path      │
   └──────────────────────────────────────────────────────────────────┘
```

---

## Why Rust, specifically

The choice is about the attack surface of spreadsheet parsing, not performance:

- **Memory-safe parser.** XLSX is a zip container of XML. The historical bug class
  here — heap overflows, decompression bombs, XML entity attacks — is exactly what
  a memory-safe language removes by construction.
- **Every cell is read as text.** No formula evaluation, no macro engine, no
  external-reference resolution. A spreadsheet carrying a malicious formula is
  inert: the value is a string, and it is never interpreted.
- **No JavaScript spreadsheet library anywhere in the pipeline.** The main server is
  TypeScript; deliberately keeping XLSX parsing out of that runtime removes an
  entire dependency tree from the trusted path.

## Why this is a separate binary

The platform server is **strictly query-only** against the sanctions data. This
container is the **only list-WRITE surface** in the system.

That separation is the security property. A query-only server cannot corrupt the
list even if it is fully compromised, because it holds no write path. Writes happen
only here — a short-lived, manually triggered, network-isolated container with a
write-only, prefix-scoped storage credential that has no overwrite rights.

---

## Where it halts

The checks below stop the pipeline rather than letting a bad list through.

| Risk | Control | On failure |
|---|---|---|
| Truncated or empty download | QC gate on staged person count | Halt before cutover |
| List silently collapses or balloons | ±20% person-count swing check | Halt, manual review |
| Search reads a half-written list | Staged build + atomic `ACTIVE_VERSION` flip | Old list stays active |
| Name-matching logic drifts | Golden-vector parity test | Build fails |

Smaller guards sit alongside these: size and row caps applied before parsing
begins, a SHA-256 gate that turns a re-run on an unchanged file into a no-op, and
an archive step ordered before any graph write so a storage failure leaves the
existing list untouched.

---

## The parity test

`src/normalize.rs` is a port of the platform's TypeScript name normaliser. Two
independent implementations of the same name-matching logic are a liability
unless equality is *enforced*, so it is:

```bash
cargo test --locked    # tests/parity.rs asserts equality against the fixtures
```

The fixtures in `testdata/` are the contract — 32 curated vectors covering the
cases most likely to diverge between two Unicode implementations: Greek
final-sigma, Turkish dotted İ, German ß/ẞ, Arabic harakat and combining marks,
Cyrillic, CJK, and Arabic-Indic digits. A second suite covers the ledger hashing.

**Why it's worth a dedicated test:** if the two normalisers diverge, a name that
the ingest pipeline indexes one way and the server searches for another way simply
will not match. There is no error, no exception, no alert — the screening check
quietly returns nothing. That silent-miss case is the one worth spending a test
suite on, so any drift fails the build.

The same reasoning covers the ledger: `src/chain.rs` reproduces the server's
canonical-JSON + SHA-256 + `prevHash` scheme, so the server's existing
`verifyChain()` can validate ingest events without the container ever connecting
to the server.

---

## Quickstart

```bash
git clone https://github.com/Rohcode/sanctions-ingest && cd sanctions-ingest
cargo test --locked                    # parity + hash-chain vectors
cargo build --release --locked
./target/release/sanctions-ingest --file <path.xlsx> --out records.json
```

Get a real input file from DFAT's
[Consolidated List page](https://www.dfat.gov.au/international-relations/security/sanctions/consolidated-list)
— the published XLSX works as-is, no preprocessing.

Parse-only mode needs no credentials, no network, and no infrastructure — it reads
a file and writes JSON. Output summary:

```json
{
  "sourceFileSha": "9f2b…",
  "sizeBytes": 1348221,
  "rowCount": 11042,
  "personCount": 3835
}
```

Each record carries the full listing: names (with aliases and original-script
forms), normalised and phonetic match keys, dates and places of birth,
citizenships, addresses, designation instrument, and the four sanction flags.

`--locked` builds exactly the committed `Cargo.lock` and fails on any dependency
drift.

### Hardened container run

```bash
export SANCTIONS_INGEST_IMAGE=sanctions-ingest@sha256:<digest>
./run.sh /path/to/dfat.xlsx records.json
```

A digest-pinned image reference is required — `run.sh` refuses to run without one,
and tags like `:latest` are not accepted.

| Control | Flag |
|---|---|
| Distroless base, non-root | `USER 65532:65532` |
| Read-only root filesystem | `--read-only` + `noexec` tmpfs |
| Drop all capabilities | `--cap-drop=ALL` |
| No privilege escalation | `--security-opt no-new-privileges` |
| Seccomp | Docker default profile (never disabled) |
| Resource bounds | `--memory=512m --pids-limit=128` |
| **No network at all** | `--network=none` |

`--network=none` is the notable one: for a pure parse run the container has no
reason to reach the network, so it is denied outright rather than merely filtered.

Commit mode reads all credentials from the environment — **nothing is ever baked
into the image**.

---

## Supply-chain controls

Every build is `--locked` — it uses exactly the committed `Cargo.lock` and fails on
any dependency drift. `cargo audit` checks the dependency tree against the RustSec
advisory database, and a CycloneDX SBOM is generated alongside the binary.

Base images are **digest-pinned**, not tag-pinned: the Dockerfile references
`rust:1.83-bookworm@sha256:…` and `gcr.io/distroless/cc-debian12:nonroot@sha256:…`,
so a rebuild fetches the exact bytes that were reviewed. A moving tag like `:latest`
would silently change what ships. An unpinned base image is treated as a build
failure, not a review comment — CI fails the build if either digest is missing.

---

## Repo map

| Path | What's in it |
|---|---|
| `src/parse.rs` | XLSX → records. Schema mapping, caps, suffix-stripped grouping. |
| `src/normalize.rs` | Name normalisation + Soundex. The parity-locked module. |
| `src/chain.rs` | Canonical JSON, SHA-256, `prevHash` ledger chaining. |
| `src/neo.rs` | Staged version, QC counts, atomic cutover, event append. |
| `src/r2.rs` | Content-addressed archive on a write-only, no-overwrite credential. |
| `src/main.rs` | CLI, pre-parse caps, the `--commit` transaction. |
| `tests/parity.rs` | Rust ≡ TypeScript normaliser, against golden vectors. |
| `tests/hash_parity.rs` | Rust ≡ TypeScript ledger hashing. |
| `testdata/` | 32 adversarial Unicode vectors + hash vectors, and their generators. |
| `run.sh` | The container hardening set, applied at run time. |
| `Dockerfile` | Multi-stage, distroless, digest-pinned base images. |
| `.github/workflows/` | Locked build, tests, advisory scan, SBOM, digest-pin check. |

---

## Status

Written as the ingest layer of an AML/CTF compliance platform, and extracted here
as a standalone reference after that platform was wound down. The full pipeline —
parse, archive, staged version, QC gate, cutover, and ledger — was built and run
end to end against live Neo4j and object storage.

Parse mode is self-contained: clone it, run the tests, point it at an XLSX. The
`--commit` path is complete and functional but expects a configured graph database
and bucket that aren't part of this repository, so it can't be exercised from a
clean checkout alone.

## License

MIT — see [LICENSE](LICENSE).
