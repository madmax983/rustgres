#!/usr/bin/env bash
# Vendor PostgreSQL 19's full regression suite into tests/conformance/pg19/.
#
# Copies, for every test in REL_19_STABLE's parallel_schedule:
#   sql/<name>.sql, expected/<name>.out (the primary expected file only;
#   platform/locale alternates like <name>_1.out are not used by the
#   runner), plus the data/ files that test_setup.sql and friends load.
#
# Usage: tests/conformance/vendor_pg19.sh [COMMIT]
#   COMMIT defaults to the pin in tests/conformance/pg19/UPSTREAM.
#   Pass a new commit (or "REL_19_STABLE") to bump the pin.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEST="$HERE/pg19"
PIN_FILE="$DEST/UPSTREAM"
REF="${1:-$( [ -f "$PIN_FILE" ] && sed -n 's/^commit: //p' "$PIN_FILE" || echo REL_19_STABLE)}"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

git clone -q --filter=blob:none --no-checkout https://github.com/postgres/postgres.git "$WORK/pg"
git -C "$WORK/pg" sparse-checkout set --no-cone \
    /src/test/regress/sql/ /src/test/regress/expected/ \
    /src/test/regress/data/ /src/test/regress/parallel_schedule
git -C "$WORK/pg" checkout -q "$REF"
COMMIT="$(git -C "$WORK/pg" rev-parse HEAD)"
SRC="$WORK/pg/src/test/regress"

rm -rf "$DEST/sql" "$DEST/expected" "$DEST/data"
mkdir -p "$DEST/sql" "$DEST/expected" "$DEST/data"
cp "$SRC/parallel_schedule" "$DEST/parallel_schedule"
cp "$SRC"/data/* "$DEST/data/"
for t in $(sed -n 's/^test: *//p' "$SRC/parallel_schedule"); do
    cp "$SRC/sql/$t.sql" "$DEST/sql/"
    cp "$SRC/expected/$t.out" "$DEST/expected/"
done

cat > "$PIN_FILE" <<EOF
source: https://github.com/postgres/postgres (src/test/regress)
branch: REL_19_STABLE
commit: $COMMIT
vendored: $(date -u +%Y-%m-%d)
EOF
echo "vendored $(ls "$DEST/sql" | wc -l) suites at $COMMIT"
