# Getting coal into Debian and Ubuntu

Ubuntu's universe takes new packages from Debian unstable automatically, so
the route into Ubuntu is: **get coal into Debian, and Ubuntu picks it up.**
For Ubuntu 27.04 that has to happen before the Debian Import Freeze on
**25 February 2027**; later than that, it lands in 27.10. Releases that are
already out (24.04 LTS, 26.04 LTS) never get new packages that way, so for
them there is a PPA build.

Everything lives in this repository, and GitHub does the work:
`.github/workflows/debian.yml` builds and tests the packages on every push,
and for a release it signs them and uploads them to the PPA and to
mentors.debian.net. Debian doesn't need the packaging to live on Salsa; the
`Vcs-Git` field points here.

| Path | What it is |
|---|---|
| `debian/` | The Debian packaging. Builds only against crates already in Debian. |
| `ppa/build-ppa-source.sh` | Makes a Launchpad PPA source package for one Ubuntu release, with the crates bundled. |
| `ppa/rules` | The `debian/rules` the PPA build uses instead (vendored, offline, chosen toolchain). |
| `ci/debian.sh` | The steps the workflow runs: upstream tarball, source package, build and test, sign and upload. Each works by hand too. |
| `tools/bump-debian-changelog.sh` | Run by cargo-release (`release.toml`): adds the `debian/changelog` entry to every release commit. |
| `tools/check-debian-deps.sh` | Checks whether a Debian or Ubuntu release has every crate coal needs. |
| `submission/ITP.txt` | The "Intent To Package" bug report, ready to send. |
| `submission/RFS.txt` | The "Request For Sponsor" bug report, ready to send after the upload to mentors. |

## Status, 23 October 2026

Done and tested:

* **Dependencies.** After the two upstream changes below, every crate coal
  needs (357 packages, including everything they depend on) is in Debian
  unstable, Ubuntu 26.10 and Ubuntu 27.04. coal builds and passes its whole
  test suite using only those crate versions.
* **Debian package**, built from the `coal-v1.0.0` tag against Debian
  unstable's crates: all tests pass during the build, lintian (pedantic) is
  clean apart from the ITP bug number that doesn't exist yet, and the three
  autopkgtests pass against the installed packages.
* **PPA packages** for Ubuntu 24.04, 26.04 and 26.10, built with the network
  cut off: tests and autopkgtests pass on each.
* **The workflow's steps**, run locally: the source job, the four test jobs,
  and signing plus a simulated upload with a throwaway key. The real upload
  needs your key, so the first real run is its test.
* **Accounts:** GPG key, mentors.debian.net, Launchpad and the PPA
  `ppa:coal-config/coal` are set up.

The copyright is yours (2026, Yannick Walen), as `debian/copyright` says.

## Upstream changes in 1.0.0

Two of these are required for Debian; one is a bug fix; the rest are what
Debian asks of upstream.

* **coal-lsp now uses `tower-lsp-server` 0.23 instead of `tower-lsp` 0.20.**
  `tower-lsp` is unmaintained and not in Debian; `tower-lsp-server` is its
  community fork, and it is in Debian. The port changes `Url` to `Uri`, drops
  `#[async_trait]`, and handles `to_file_path()` now returning an `Option`.
  The language server's answers are unchanged (same capabilities, same
  diagnostics). One behaviour changed for the better: the server now exits on
  the LSP `exit` notification. With `tower-lsp` 0.20 it kept running until the
  editor closed its stdin.
* **minijinja requirement lowered from `2.24.0` to `2.19`.** Debian has
  2.19.0. The full test suite passes on 2.19.0, and `coal`, `coal -v` and
  `coal -s staging` on `example/` print byte-identical output. The lock file
  still uses 2.24.0 for your own builds.
* **Fix: `coal` and coal-lsp build a configuration that selects itself.**
  When the configuration settles the selection on its own (`node.yaml`
  holding `cop01`, say), `coal` stopped with "The structure declares node but
  nothing matches", and the editor showed the same error. The build that
  lists the choices had already merged that selection, and dropped the paths
  it then looked for. Now that build is recognised as the configuration
  itself. 10 of the 11 `_structure.yaml` fixtures were affected; they pass
  through the command line now, and `coal/tests/compile_configurations_tests.rs`
  runs every build fixture the way `coal` and coal-lsp build it, so the
  harness and the command line can't drift apart again.
* `serde_yaml = "0.9.34+deprecated"` became `"0.9.34"`. Cargo ignores build
  metadata in requirements and warned about it.
* Cargo metadata: `description`, `license`, `repository`, `homepage`,
  `authors`, `keywords`, `categories` and `rust-version = "1.89"`. coal uses
  let-chains (stable since Rust 1.88). 1.85 fails; 1.89 is the oldest
  version I could test, and passes everything.
* Man pages: `doc/man/coal.1` and `doc/man/coal-lsp.1`.
* `.gitattributes` keeps `packaging/` and `doc/man/` in LF line endings, even
  in a Windows checkout. Debian's tools need that.

## Ubuntu releases at a glance

| Release | Rust in the archive | Crates in the archive | How coal gets there |
|---|---|---|---|
| 27.04 (tenacious) | 1.97 | all | from Debian (autosync until 25 Feb 2027) |
| 26.10 (stonking) | 1.97 | all | PPA (released; no new packages) |
| 26.04 LTS (resolute) | 1.93 | clap 4.5, minijinja 2.12, regex 1.12.2, tower-lsp-server 0.22, no marked-yaml | PPA, crates bundled |
| 24.04 LTS (noble) | 1.75 (`rustc-1.89` available) | far too old | PPA, crates bundled, Rust 1.89 |

## Step by step

### 1. Give the workflow your signing key

Export the key you registered on Launchpad and mentors.debian.net (`gpg -K`
lists it):

```sh
gpg --armor --export-secret-keys <KEY-ID>
```

In GitHub, open the repository's *Settings > Secrets and variables >
Actions* and add:

* `DEBIAN_SIGNING_KEY`: the whole output, from `-----BEGIN` to `END...-----`.
* `DEBIAN_SIGNING_PASSPHRASE`: its passphrase, if it has one.

Anyone who can change the workflows can use that key, so keep write access
to the repository to people you trust with it. To keep your main key off
GitHub, give it a signing subkey and export only that
(`gpg --armor --export-secret-subkeys <SUBKEY-ID>!`). Then send the public
key to the keyserver again, so Launchpad and mentors see the new subkey.

In the PPA's settings on Launchpad, tick the architectures you want (amd64
is on by default; arm64 is worth adding).

### 2. Publish 1.0.0 to the PPA

In GitHub: *Actions > Debian and Ubuntu packages > Run workflow*, with tag
`coal-v1.0.0` and "Where to upload" set to `ppa`. It builds, tests, signs
and uploads the packages for 24.04, 26.04 and 26.10. Launchpad emails you
when it accepts each one and builds them in roughly half an hour. Then
anyone can install coal with:

```sh
sudo add-apt-repository ppa:coal-config/coal
sudo apt install coal coal-lsp
```

### 3. File the ITP

Edit the "Why package it" paragraph in `submission/ITP.txt` into your own
words, then send it from the address in `Maintainer:`. The bug tracker
replies with a number. Put it in `debian/changelog` in place of `#ITPBUG`
(`Closes: #1234567`), then commit and push.

### 4. Upload to mentors and ask for a sponsor

Run the workflow again for `coal-v1.0.0`, now with "Where to upload" set to
`mentors`. (While the changelog still says `#ITPBUG`, the workflow refuses
to upload to mentors and says why.) Then:

* Send `submission/RFS.txt`.
* Say hello on the Rust team's list (debian-rust@lists.debian.org) or Matrix
  room (#debian-rust:matrix.org) and point them at the RFS. The Rust team
  reviews Rust packages fastest.
* Changes the sponsor asks for go into `packaging/debian/`. Raise the
  revision with a new changelog entry (`1.0.0-2`), push, and run the
  workflow for `coal-v1.0.0` with `mentors` again.

### 5. NEW queue, testing, Ubuntu

The ftpmasters review licensing and copyright for every new package; expect
weeks to a few months. Once accepted, coal migrates to testing after about
five days if nothing breaks, and Ubuntu's devel series imports it
automatically.

### 6. Every later release

Run *Release COAL Binaries* as you do now. cargo-release adds the
`debian/changelog` entry to the release commit, and when the binaries are
out the workflow builds, tests and uploads the new version to the PPA and to
mentors. Then ask your sponsor to upload it. After a few sponsored uploads
you can apply to become a Debian Maintainer, which lets you upload coal
yourself; then point the workflow straight at Debian, and a release needs
nothing more from you.

Git on Windows doesn't record executable bits here (`core.filemode` is off),
so a new script has to be added with the bit set:

```sh
git add --chmod=+x path/to/script.sh
```

## Before every later release

* Run `packaging/tools/check-debian-deps.sh sid`. If a new dependency, or a
  raised minimum version, isn't in Debian yet, it says which.
* Upstream minijinja is at 3.0.1 already. When Debian moves to 3.x, coal's
  `^2` requirement stops building there; port to 3.x around that time.

## Things found along the way

* Two empty, untracked folders, `coal/tests/build/include_directory_optional_exists`
  and `..._missing`, make `cargo test` fail locally ("missing
  `_expected.yaml`"). Git doesn't keep empty folders, so clean checkouts are
  fine.
* `install_private.sh` was for the private repository and can go now.
* **The old history is still public through pull request #1.** GitHub keeps
  `refs/pull/1/head` of the merged "Feat/jinja engine" PR even after the
  branch is gone, and `git fetch origin pull/1/head` returns 72 commits of
  the old history. Only GitHub Support can remove it (ask them to remove the
  pull request and run a garbage collection), short of deleting and
  recreating the repository.
