#!/bin/sh
# cargo-release pre-release hook (see release.toml): adds the debian/changelog
# entry for the version being released, so the release commit carries it.
#
# cargo-release runs it once per crate with the same version, so it does
# nothing when the entry is already there. A pre-release such as 1.1.0-rc.1
# becomes 1.1.0~rc.1, which Debian sorts before 1.1.0.
set -eu

root="${WORKSPACE_ROOT:-$(git rev-parse --show-toplevel)}"
version="$(printf '%s' "${NEW_VERSION:?cargo-release sets NEW_VERSION}" | sed 's/-/~/')"
changelog="$root/packaging/debian/changelog"

if [ "${DRY_RUN:-false}" = "true" ]; then
    echo "would add coal ($version-1) to $changelog"
    exit 0
fi

top="$(sed -n '1s/^[^(]*(\([^)]*\)).*/\1/p' "$changelog")"
case "$top" in
    "$version"-*) exit 0 ;;
esac

maintainer="$(sed -n 's/^Maintainer: //p' "$root/packaging/debian/control")"
entry="$(mktemp)"
{
    printf 'coal (%s-1) unstable; urgency=medium\n\n' "$version"
    printf '  * New upstream release.\n\n'
    printf ' -- %s  %s\n\n' "$maintainer" "$(LC_ALL=C date -R)"
    cat "$changelog"
} > "$entry"
cat "$entry" > "$changelog"
rm -f "$entry"
echo "added coal ($version-1) to packaging/debian/changelog"
