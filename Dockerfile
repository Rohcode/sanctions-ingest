# syntax=docker/dockerfile:1
# ─────────────────────────────────────────────────────────────────────────────
# sanctions-ingest — hardened one-shot DFAT Consolidated List parser.
#
# Multi-stage, distroless, DIGEST-PINNED. Artifact signing (cosign/SLSA) is not
# included; trust here rests on digest-pinning + Cargo.lock --locked + SBOM.
#
# Pin digests before first build (network step, do once, commit the result):
#   docker manifest inspect rust:1.83-bookworm | jq -r '.manifests[0].digest'   # or the index digest
#   docker manifest inspect gcr.io/distroless/cc-debian12:nonroot | jq -r '.digest'
# then replace the two <sha256:…> placeholders below. CI must fail if either is
# still a placeholder (see .github/workflows/ci.yml).
# ─────────────────────────────────────────────────────────────────────────────

FROM rust:1.83-bookworm@sha256:a45bf1f5d9af0a23b26703b3500d70af1abff7f984a7abef5a104b42c02a292b AS build
WORKDIR /src
# Cache deps: copy manifests, fetch with the committed lockfile, then sources.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
COPY testdata ./testdata
# --locked: build EXACTLY the committed Cargo.lock; fail on any drift.
RUN cargo build --release --locked

# Distroless cc (glibc + libgcc for the Rust binary), nonroot variant.
FROM gcr.io/distroless/cc-debian12:nonroot@sha256:bd2899c12b335c827750ccf2359879eab09c09b206023dcebea408947d54127c
COPY --from=build /src/target/release/sanctions-ingest /usr/local/bin/sanctions-ingest
# nonroot image already runs as 65532; restate for clarity + defence in depth.
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/sanctions-ingest"]
