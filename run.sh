#!/usr/bin/env bash
# Hardened one-shot run wrapper. Operator downloads + verifies the DFAT
# XLSX, then runs this. All flags are the day-1 "cheap, all kept" hardening set.
#
#   ./run.sh /path/to/dfat.xlsx [/path/to/records.json]
#
# Requires: a digest-pinned image tag in $SANCTIONS_INGEST_IMAGE (no :latest).
set -euo pipefail

IMAGE="${SANCTIONS_INGEST_IMAGE:?set SANCTIONS_INGEST_IMAGE to a digest-pinned ref, e.g. sanctions-ingest@sha256:...}"
FILE="${1:?usage: run.sh <xlsx> [out.json]}"
OUT="${2:-}"

# Mount the input read-only at a fixed path inside the container.
ARGS=(--file /in/dfat.xlsx)
MOUNTS=(-v "$(cd "$(dirname "$FILE")" && pwd)/$(basename "$FILE")":/in/dfat.xlsx:ro)
if [[ -n "$OUT" ]]; then
  touch "$OUT"
  ARGS+=(--out /out/records.json)
  MOUNTS+=(-v "$(cd "$(dirname "$OUT")" && pwd)/$(basename "$OUT")":/out/records.json)
fi

# Note: Docker applies its built-in default seccomp profile automatically (we do
# NOT pass --security-opt seccomp=unconfined, so it stays on). To pin a specific
# profile instead, add `--security-opt seccomp=/path/to/profile.json`.
exec docker run --rm \
  --read-only --tmpfs /tmp:rw,size=64m,noexec \
  --cap-drop=ALL \
  --security-opt no-new-privileges \
  --memory=512m --pids-limit=128 \
  --network=none \
  --user 65532:65532 \
  "${MOUNTS[@]}" \
  "$IMAGE" "${ARGS[@]}"
