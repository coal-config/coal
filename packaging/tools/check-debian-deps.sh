#!/bin/bash
# Check that every crate coal needs is in a Debian or Ubuntu release, at a
# version coal's Cargo.toml accepts, including everything those crates need.
#
#   packaging/tools/check-debian-deps.sh [suite]
#
#   suite  a Debian suite (sid, forky, trixie; default sid) or an Ubuntu
#          series (tenacious, stonking, resolute, noble)
#
# Needs cargo, curl, xz-utils and python3-apt. Exits non-zero when something
# is missing, and prints what.
set -euo pipefail

suite="${1:-sid}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
cache="$(mktemp -d)"
trap 'rm -rf "${cache:?}"' EXIT

case "$suite" in
    sid|unstable|forky|testing|trixie|stable|bookworm)
        urls=("http://deb.debian.org/debian/dists/$suite/main/binary-amd64/Packages.xz") ;;
    *)
        urls=()
        for pocket in "$suite" "$suite-updates"; do
            for component in main universe; do
                urls+=("http://archive.ubuntu.com/ubuntu/dists/$pocket/$component/binary-amd64/Packages.xz")
            done
        done ;;
esac

indexes=()
for url in "${urls[@]}"; do
    file="$cache/$(echo "$url" | tr '/:' '__')"
    if curl -fsSL "$url" | xz -d > "$file" 2> /dev/null && [ -s "$file" ]; then
        indexes+=("$file")
    fi
done
[ "${#indexes[@]}" -gt 0 ] || { echo "could not download any package index for '$suite'" >&2; exit 2; }

cargo metadata --format-version 1 --filter-platform x86_64-unknown-linux-gnu \
    --manifest-path "$repo/Cargo.toml" > "$cache/metadata.json"
python3 "$here/debian_closure.py" "$cache/metadata.json" "${indexes[@]}" > "$cache/result.json"

python3 - "$cache/result.json" "$suite" <<'PY'
import json, sys
result, suite = json.load(open(sys.argv[1])), sys.argv[2]
missing = {}
for d in result["direct"]:
    if not d["satisfied"]:
        name = d["relation"].split("+")[0].removesuffix("-dev")
        missing.setdefault(name, (d["cargo_req"], d["archive"]))
for name, (req, have) in sorted(missing.items()):
    print(f"MISSING  {name}: Cargo.toml wants {req}, {suite} has {have or 'nothing'}")
for p in result["problems"]:
    if not any(p["relation"].startswith(f"{m}") for m in missing):
        print(f"MISSING  {p['relation']} {p['bounds']} (needed by {p['needed_by']}), {suite} has {', '.join(p['archive_has']) or 'nothing'}")
if missing or result["problems"]:
    sys.exit(1)
print(f"OK: all {result['closure_size']} packages coal needs are in {suite} at compatible versions.")
PY
