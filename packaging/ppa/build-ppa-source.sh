#!/bin/bash
# Build an Ubuntu source package of coal for a Launchpad PPA.
#
#   packaging/ppa/build-ppa-source.sh <series> [ppa-revision]
#
#   series        noble (24.04 LTS), resolute (26.04 LTS), stonking (26.10)
#                 or tenacious (27.04)
#   ppa-revision  bump it to re-upload the same coal version (default 1)
#
# The version comes from packaging/debian/changelog: 1.0.0-1 becomes
# 1.0.0-1~ppa1~ubuntu24.04. It needs git, cargo, dpkg-dev, devscripts, python3
# and xz-utils, and network access once, to download the crates it bundles.
# The GitHub workflow .github/workflows/debian.yml runs it and uploads the
# result; by hand, packaging/ci/debian.sh upload does the signing and upload.
#
# The crates go into a second upstream tarball (coal_<v>.orig-vendor.tar.xz),
# so Launchpad builds offline with exactly the versions in Cargo.lock. The
# Debian packaging from packaging/debian is used as it is, except that:
#   * debian/rules is packaging/ppa/rules (vendored, toolchain chosen below),
#   * the librust-*-dev Build-Depends are dropped,
#   * debian/copyright gains a stanza for every bundled crate.
#
# Environment:
#   ORIG=<file>   use this upstream tarball instead of `git archive HEAD`
#                 (pass the GitHub tag tarball, coal-v<version>.tar.gz, to
#                 match what Debian uses)
#   OUT=<dir>     where to put the results (default: target/ppa)
set -euo pipefail

series="${1:?usage: $0 <noble|resolute|stonking|tenacious> [ppa-revision]}"
revision="${2:-1}"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
changelog="$repo/packaging/debian/changelog"
debian_version="$(dpkg-parsechangelog -l "$changelog" -S Version)"
version="${debian_version%-*}"
# Fixed timestamps keep the vendor tarball byte-identical between runs.
timestamp="$(dpkg-parsechangelog -l "$changelog" -S Timestamp)"

# The oldest rustc coal builds with is 1.89. The two LTS releases default to
# older ones (24.04: 1.75) or have debhelper 13, so they get a versioned
# toolchain and compat 13.
case "$series" in
    noble)     ubuntu=24.04; compat=13; rust_bin=/usr/lib/rust-1.89/bin
               toolchain=" cargo-1.89,\n rustc-1.89," ;;
    resolute)  ubuntu=26.04; compat=13; rust_bin=/usr/bin
               toolchain=" cargo (>= 1.89),\n rustc (>= 1.89)," ;;
    stonking)  ubuntu=26.10; compat=14; rust_bin=/usr/bin
               toolchain=" cargo (>= 1.89),\n rustc (>= 1.89)," ;;
    tenacious) ubuntu=27.04; compat=14; rust_bin=/usr/bin
               toolchain=" cargo (>= 1.89),\n rustc (>= 1.89)," ;;
    *) echo "unknown series '$series' (noble, resolute, stonking or tenacious)" >&2; exit 1 ;;
esac

out="$(mkdir -p "${OUT:-$repo/target/ppa}" && cd "${OUT:-$repo/target/ppa}" && pwd)"
work="$out/$series"
src="$work/coal-$version"
orig="$out/coal_$version.orig.tar.gz"
vendor_tarball="$out/coal_$version.orig-vendor.tar.xz"
mkdir -p "$out"

# 1. Upstream tarball, shared by every series.
if [ -n "${ORIG:-}" ]; then
    cp "$ORIG" "$orig"
elif [ ! -f "$orig" ]; then
    git -C "$repo" archive --format=tar.gz --prefix="coal-$version/" -o "$orig" HEAD
fi

# 2. Vendored crates, shared by every series.
if [ ! -f "$vendor_tarball" ]; then
    staging="$(mktemp -d)"
    mkdir "$staging/unpacked"
    tar -xzf "$orig" -C "$staging/unpacked" --strip-components=1
    (cd "$staging/unpacked" && cargo vendor --locked --versioned-dirs "$staging/vendor" \
        > /dev/null 2> "$staging/vendor.log") || { cat "$staging/vendor.log" >&2; exit 1; }
    tar -C "$staging" --sort=name --owner=0 --group=0 --numeric-owner \
        --mtime="@$timestamp" \
        -cJf "$vendor_tarball" vendor
    rm -rf "${staging:?}"
fi

# 3. Unpack, add the packaging, adapt it for this series. dpkg-source wants
#    the upstream tarballs next to the source tree.
rm -rf "${src:?}"
mkdir -p "$src"
cp -f "$orig" "$vendor_tarball" "$work/"
tar -xzf "$orig" -C "$src" --strip-components=1
tar -xJf "$vendor_tarball" -C "$src"
cp -r "$repo/packaging/debian" "$src/debian"
sed "s|@RUST_BIN@|$rust_bin|" "$here/rules" > "$src/debian/rules"
chmod 0755 "$src/debian/rules"

python3 - "$src" "$compat" "$toolchain" <<'EOF'
import json, re, subprocess, sys
src, compat, toolchain = sys.argv[1], sys.argv[2], sys.argv[3].replace("\\n", "\n")

path = f"{src}/debian/control"
control = open(path).read()
control = re.sub(r"^ librust-.*\n", "", control, flags=re.M)
control = re.sub(r"^ dh-cargo .*\n", "", control, flags=re.M)
control = re.sub(r"^ cargo:native .*\n rustc:native .*\n libstd-rust-dev,\n python3:native,\n",
                 toolchain + "\n", control, flags=re.M)
control = control.replace("debhelper-compat (= 14)", f"debhelper-compat (= {compat})")
if compat == "13":
    control = control.replace("Standards-Version:", "Rules-Requires-Root: no\nStandards-Version:")
open(path, "w").write(control)

metadata = json.loads(subprocess.run(
    ["cargo", "metadata", "--format-version", "1", "--offline", "--locked",
     "--config", f"source.crates-io.replace-with='vendored'",
     "--config", f"source.vendored.directory='{src}/vendor'"],
    cwd=src, capture_output=True, text=True, check=True).stdout)
workspace = set(metadata["workspace_members"])
stanzas, licenses = [], set()
for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"])):
    if package["id"] in workspace:
        continue
    expression = (package.get("license") or "").replace("/", " OR ")
    names = set(re.findall(r"[A-Za-z0-9.+-]+(?: WITH [A-Za-z0-9.+-]+)?", expression)) - {"OR", "AND", "WITH"}
    licenses |= {n for n in names if " WITH " not in n}
    holders = "\n ".join(package.get("authors") or [f"The {package['name']} authors"])
    stanzas.append(f"Files: vendor/{package['name']}-{package['version']}/*\n"
                   f"Copyright: {holders}\n"
                   f"License: {expression or 'UNKNOWN'}\n")
notes = ("\nComment: The crates under vendor/ are bundled for this PPA build only.\n"
         " Their licenses, as declared in each crate's Cargo.toml, are listed\n"
         " below; the full texts are in each crate's directory and, for the\n"
         " common ones, in /usr/share/common-licenses.\n"
         f" Licenses used: {', '.join(sorted(licenses))}.\n\n")
with open(f"{src}/debian/copyright", "a") as f:
    f.write(notes + "\n".join(stanzas))
EOF

# 4. A changelog entry for this series. The version sorts below the Debian
#    one, so an archive package replaces the PPA one.
ppa_version="$debian_version~ppa$revision~ubuntu$ubuntu"
maintainer="$(sed -n 's/^Maintainer: //p' "$src/debian/control")"
(cd "$src" && DEBFULLNAME="${maintainer% <*}" DEBEMAIL="$(echo "$maintainer" | sed 's/.*<\(.*\)>/\1/')" \
    dch -b --newversion "$ppa_version" --distribution "$series" --force-distribution \
        "PPA build for Ubuntu $ubuntu, with the crates bundled from Cargo.lock.")

# 5. The source package. -d: the build dependencies are for Launchpad, not
#    this machine.
(cd "$src" && dpkg-buildpackage -S -sa -d -us -uc > "$work/source-build.log" 2>&1) \
    || { cat "$work/source-build.log" >&2; exit 1; }
changes="$work/coal_${ppa_version}_source.changes"

echo
echo "Source package for $series: $changes"
echo "Sign and upload it with:"
echo "  packaging/ci/debian.sh upload ppa $changes"
