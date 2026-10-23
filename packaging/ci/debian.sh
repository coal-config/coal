#!/bin/bash
# Builds, tests and uploads the Debian and Ubuntu packages of coal. The GitHub
# workflow .github/workflows/debian.yml runs these steps; each one also works
# by hand.
#
#   debian.sh orig <tag|HEAD> <dir>
#       The upstream tarball, named for the version in debian/changelog: the
#       GitHub tarball of the tag, or `git archive` of HEAD for a test build.
#
#   debian.sh source <orig> <dir>
#       The Debian source package (unsigned), from that tarball and
#       packaging/debian.
#
#   debian.sh test <dsc> [--lintian]
#       Installs the build dependencies, builds, optionally runs lintian, and
#       runs the autopkgtests against the result. It installs packages, so run
#       it as root in a container or chroot you can throw away.
#
#   debian.sh upload <mentors|ppa> <changes>
#       Signs the upload with the key in $DEBIAN_SIGNING_KEY (an ASCII-armored
#       secret key; $DEBIAN_SIGNING_PASSPHRASE if it has one) and uploads it
#       to mentors.debian.net or to the Launchpad PPA in $PPA (default
#       coal-config/coal). DRY_RUN=1 signs but only simulates the upload.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
changelog="$repo/packaging/debian/changelog"

note() { echo "debian.sh: $*" >&2; }
fail() { echo "::error::$*" >&2; exit 1; }

debian_version() { dpkg-parsechangelog -l "$changelog" -S Version; }
upstream_version() { local v; v="$(debian_version)"; echo "${v%-*}"; }

cmd_orig() {
    local ref="${1:?ref}" dir="${2:?dir}" version tarball
    version="$(upstream_version)"
    dir="$(mkdir -p "$dir" && cd "$dir" && pwd)"
    tarball="$dir/coal_$version.orig.tar.gz"
    if [ "$ref" = "HEAD" ]; then
        git -C "$repo" archive --format=tar.gz --prefix="coal-$version/" -o "$tarball" HEAD
    else
        curl -fsSL -o "$tarball" \
            "https://github.com/${GITHUB_REPOSITORY:-coal-config/coal}/archive/refs/tags/$ref.tar.gz"
        # The packaging and the tag have to be about the same version.
        local tagged
        tagged="$(tar -xzOf "$tarball" --wildcards '*/Cargo.toml' 2>/dev/null \
            | sed -n 's/^version = "\(.*\)"\r\{0,1\}$/\1/p' | head -n 1 | sed 's/-/~/')"
        [ "$tagged" = "$version" ] || fail "$ref is version '$tagged', but packaging/debian/changelog is for $version. Add a changelog entry for it first."
    fi
    echo "$tarball"
}

cmd_source() {
    local orig="${1:?orig tarball}" dir="${2:?dir}" version src
    version="$(upstream_version)"
    dir="$(mkdir -p "$dir" && cd "$dir" && pwd)"
    src="$dir/coal-$version"
    [ "$(readlink -f "$orig")" = "$(readlink -f "$dir/coal_$version.orig.tar.gz")" ] \
        || cp "$orig" "$dir/coal_$version.orig.tar.gz"
    rm -rf "${src:?}"
    mkdir -p "$src"
    tar -xzf "$dir/coal_$version.orig.tar.gz" -C "$src" --strip-components=1
    cp -r "$repo/packaging/debian" "$src/debian"
    (cd "$src" && dpkg-buildpackage -S -sa -d -us -uc > "$dir/source-build.log" 2>&1) \
        || { cat "$dir/source-build.log" >&2; fail "building the source package failed"; }
    echo "$dir/coal_$(debian_version)_source.changes"
}

cmd_test() {
    local dsc="${1:?dsc}" lintian="${2:-}" work
    dsc="$(readlink -f "$dsc")"
    export DEBIAN_FRONTEND=noninteractive
    # equivs signs its dummy package with these; without them it makes up an
    # address from the hostname, which newer dpkg refuses.
    export DEBFULLNAME="${DEBFULLNAME:-coal CI}" DEBEMAIL="${DEBEMAIL:-ci@localhost}"
    apt-get update -qq
    apt-get install -y -qq --no-install-recommends \
        build-essential devscripts equivs fakeroot autopkgtest python3-yaml \
        ${lintian:+lintian} > /dev/null

    work="$(mktemp -d)"
    dpkg-source -x "$dsc" "$work/src" > /dev/null
    # From the parent directory: mk-build-deps leaves its own files behind,
    # and dpkg-source would refuse a tree with stray files in it.
    (cd "$work" && mk-build-deps -i -r -t "apt-get -y -qq --no-install-recommends" src/debian/control > /dev/null)
    (cd "$work/src" && dpkg-buildpackage -us -uc -b)

    if [ -n "$lintian" ]; then
        lintian --profile debian --fail-on error --info --pedantic "$work"/coal_*_"$(dpkg --print-architecture)".changes
    fi
    autopkgtest --output-dir "$work/autopkgtest" "$work"/*.deb "$work/src" -- null
    note "build and autopkgtests passed for $(basename "$dsc")"
}

# Imports the key into a keyring of its own and sets FINGERPRINT.
import_signing_key() {
    [ -n "${DEBIAN_SIGNING_KEY:-}" ] || fail "DEBIAN_SIGNING_KEY is not set"
    GNUPGHOME="$(mktemp -d)"
    export GNUPGHOME
    chmod 700 "$GNUPGHOME"
    printf '%s\n' "$DEBIAN_SIGNING_KEY" | gpg --batch --quiet --import
    if [ -n "${DEBIAN_SIGNING_PASSPHRASE:-}" ]; then
        echo allow-preset-passphrase > "$GNUPGHOME/gpg-agent.conf"
        gpgconf --kill gpg-agent
        gpg-connect-agent /bye > /dev/null 2>&1
        local preset="" candidate grip
        for candidate in /usr/lib/gnupg/gpg-preset-passphrase /usr/libexec/gpg-preset-passphrase \
            /usr/lib/gnupg2/gpg-preset-passphrase; do
            [ -x "$candidate" ] && { preset="$candidate"; break; }
        done
        [ -n "$preset" ] || fail "gpg-preset-passphrase not found"
        for grip in $(gpg --batch --with-colons --with-keygrip -K | awk -F: '/^grp:/ { print $10 }'); do
            "$preset" --preset -P "$DEBIAN_SIGNING_PASSPHRASE" "$grip"
        done
    fi
    FINGERPRINT="$(gpg --batch --with-colons -K | awk -F: '/^fpr:/ { print $10; exit }')"
    [ -n "$FINGERPRINT" ] || fail "DEBIAN_SIGNING_KEY holds no secret key"
}

cmd_upload() {
    local target="${1:?mentors or ppa}" changes="${2:?changes}" config ppa
    ppa="${PPA:-coal-config/coal}"
    case "$target" in
        mentors)
            if grep -q 'ITPBUG' "$changes"; then
                echo "::warning::Not uploading to mentors: debian/changelog still says #ITPBUG. File the ITP and put its bug number there."
                return 0
            fi ;;
        ppa) ;;
        *) fail "upload target must be mentors or ppa" ;;
    esac

    import_signing_key
    debsign --no-conf --re-sign -k"$FINGERPRINT" "$changes" \
        || fail "signing failed. If the key has a passphrase, set DEBIAN_SIGNING_PASSPHRASE."
    # Launchpad and mentors check the signature against the key registered
    # there; this keyring does not trust the key, so dput would complain.

    config="$(mktemp)"
    cat > "$config" <<EOF
[mentors]
fqdn = mentors.debian.net
incoming = /upload
method = https
allow_unsigned_uploads = 0
progress_indicator = 0
allowed_distributions = .*

[ppa]
fqdn = ppa.launchpad.net
method = ftp
incoming = ~${ppa%%/*}/ubuntu/${ppa#*/}/
login = anonymous
passive_ftp = 1
allow_unsigned_uploads = 0
EOF
    if [ "${DRY_RUN:-0}" = "1" ]; then
        dput -u --simulate -c "$config" "$target" "$changes"
    else
        dput -u -c "$config" "$target" "$changes"
    fi
}

command="${1:-}"
shift || true
case "$command" in
    orig|source|test|upload) "cmd_$command" "$@" ;;
    *) sed -n '2,25p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
