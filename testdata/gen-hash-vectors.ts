/**
 * Hash-parity golden vectors. Emits stableStringify / computePayloadHash
 * / computeEventHash outputs from the SERVER's recordEvent.ts so the Rust port
 * (src/chain.rs) can prove byte-identical hashing. If the IngestEvent chain hashes
 * differently from the server, the server's verifyChain() would reject a valid
 * chain — so this parity is as load-bearing as the name-normalisation parity.
 *
 * NOTE: this generator imports the TypeScript platform server's recordEvent
 * module, which lives in a separate private repository and is NOT part of this
 * repo. The script is kept here to document exactly how the fixture was produced
 * and to make regeneration reproducible for anyone who has that server checked
 * out. It will not run standalone; adjust the import below to point at your
 * checkout.
 *
 * The committed `hash-vectors.json` IS the contract — `cargo test` replays it and
 * needs nothing from TypeScript.
 *
 * Run (with the server repo checked out alongside this one):
 *   npx tsx testdata/gen-hash-vectors.ts
 */
import { writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import {
  stableStringify,
  computePayloadHash,
  computeEventHash,
  GENESIS_HASH,
  type EventHashMaterial,
} from '../../server/src/services/provenance/recordEvent.js';

// stableStringify probes — type/escaping/key-order hazards.
const stableInputs: unknown[] = [
  null,
  42,
  -7,
  'hello',
  'quote " and \\ backslash',
  true,
  false,
  ['b', 'a', 'c'],
  { b: 1, a: 2, c: 3 },
  { z: { y: [3, 2, 1], x: 'k' }, a: null },
  'unicode: Müller 金正恩 ١٢٣',
];

// Representative IngestEvent payloads (committed via payloadHash).
const payloads: unknown[] = [
  {},
  { sourceSha: 'a'.repeat(64), added: 10, updated: 2, deactivated: 1, unchanged: 3820, skipped: false, rowCount: 11042, personCount: 3835 },
  { sourceSha: 'b'.repeat(64), added: 0, updated: 0, deactivated: 0, unchanged: 0, skipped: true, rowCount: 0, personCount: 0 },
];

// EventHashMaterial probes — chain links incl. genesis.
const materials: EventHashMaterial[] = [
  {
    seq: 0,
    chainId: 'sanctions-ingest-dfat',
    eventType: 'SANCTIONS_INGEST',
    action: 'CREATE',
    objectType: 'ListVersion',
    objectId: 'v-0001',
    entityId: null,
    actorAccountId: null,
    citations: ['Act s.28(2)(e)(ii)', 'Rules s.5-3'],
    payloadHash: computePayloadHash(payloads[1]),
    prevHash: GENESIS_HASH,
  },
  {
    seq: 1,
    chainId: 'sanctions-ingest-dfat',
    eventType: 'SANCTIONS_INGEST',
    action: 'SKIP',
    objectType: 'ListVersion',
    objectId: null,
    entityId: null,
    actorAccountId: 'acct-123',
    citations: [],
    payloadHash: computePayloadHash(payloads[2]),
    prevHash: computeEventHash({
      seq: 0,
      chainId: 'sanctions-ingest-dfat',
      eventType: 'SANCTIONS_INGEST',
      action: 'CREATE',
      objectType: 'ListVersion',
      objectId: 'v-0001',
      entityId: null,
      actorAccountId: null,
      citations: ['Act s.28(2)(e)(ii)', 'Rules s.5-3'],
      payloadHash: computePayloadHash(payloads[1]),
      prevHash: GENESIS_HASH,
    }),
  },
];

const out = {
  genesisHash: GENESIS_HASH,
  stable: stableInputs.map((input) => ({ input, out: stableStringify(input) })),
  payloadHash: payloads.map((payload) => ({ payload, hash: computePayloadHash(payload) })),
  eventHash: materials.map((m) => ({ material: m, hash: computeEventHash(m) })),
};

const outPath = join(dirname(fileURLToPath(import.meta.url)), 'hash-vectors.json');
writeFileSync(outPath, JSON.stringify(out, null, 2) + '\n', 'utf8');
console.log(`wrote hash vectors → ${outPath}`);
