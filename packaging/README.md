# Getting coal into Debian and Ubuntu

Ubuntu's universe takes new packages from Debian unstable automatically, so
the route into Ubuntu is: **get coal into Debian, and Ubuntu picks it up.**
For Ubuntu 27.04 that has to happen before the Debian Import Freeze on
**25 February 2027**; later than that, it lands in 27.10. Releases that are
already out (24.04 LTS, 26.04 LTS) never get new packages that way, so for
them there is a PPA build.

This folder holds everything for both:

| Path | What it is |
|---|---|
| `debian/` | The Debian packaging. Builds only against crates already in Debian. Copy it to the Salsa repository. |
| `ppa/build-ppa-source.sh` | Makes a Launchpad PPA source package for one Ubuntu release, with the crates bundled. |
| `ppa/rules` | The `debian/rules` the PPA build uses instead (vendored, offline, chosen toolchain). |
| `tools/check-debian-deps.sh` | Checks whether a Debian or Ubuntu release has every crate coal needs. Run it before each release. |
| `submission/ITP.txt` | The "Intent To Package" bug report, ready to send. |
| `submission/RFS.txt` | The "Request For Sponsor" bug report, ready to send after uploading to mentors. |

## Status, 23 October 2026

Done and tested:

* **Dependencies.** After the two upstream changes below, every crate coal
  needs (357 packages, including everything they depend on) is in Debian
  unstable, Ubuntu 26.10 and Ubuntu 27.04. coal builds and passes its whole
  test suite using only those crate versions.
* **Debian package.** Built in a clean Ubuntu 27.04 chroot (which carries
  Debian unstable's crates), with apt resolving the Build-Depends. All tests
  pass during the build, and lintian (pedantic, Debian and Ubuntu profiles)
  is clean apart from the ITP bug number that doesn't exist yet. The three
  autopkgtests pass against the installed packages.
* **PPA package.** Built in clean Ubuntu 24.04 and 26.04 chroots with the
  network cut off, Rust 1.89 on 24.04. Tests and autopkgtests pass.

Only you can do these. The first one blocks Debian:

1. **Make `github.com/coal-config/coal` public.** Debian only takes software
   whose source anyone can download, and `debian/watch` finds new releases
   through the GitHub API. Right now the repository asks for credentials, and
   the `install.sh` URL in the README returns 404.
2. **Release coal-v0.4.8** with the upstream changes below.
3. **Get a GPG key and accounts**: a key for signing uploads, an account on
   salsa.debian.org and mentors.debian.net, and on Launchpad for the PPA.

The copyright is yours (2026, Yannick Walen), as `debian/copyright` says.

## Upstream changes in this release

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

### 1. Release upstream

Review and merge the changes. Git on Windows doesn't record executable bits
here (`core.filemode` is off), so add the scripts with them set:

```sh
git add --chmod=+x packaging/debian/rules packaging/debian/tests/build-fixtures \
    packaging/debian/tests/validate-example packaging/debian/tests/lsp-handshake \
    packaging/ppa/build-ppa-source.sh packaging/tools/check-debian-deps.sh \
    packaging/tools/debian_closure.py
git add .gitattributes doc packaging
```

Then run your release workflow for 0.4.8. The
tag must be `coal-v0.4.8`, which `debian/watch` looks for (it ignores the
`ext-v*` tags of the VS Code extension).

### 2. Set up the Salsa repository

Create the project `debian/coal` on salsa.debian.org. Any Debian contributor
may create projects in the `debian` group. Then, in a fresh directory:

```sh
git clone https://salsa.debian.org/debian/coal.git && cd coal
cp -r /path/to/coal/packaging/debian debian
git add debian && git commit -m "Initial packaging"
gbp import-orig --uscan --pristine-tar      # downloads coal-v0.4.8 from GitHub
gbp buildpackage --git-builder=sbuild -d unstable
git push --all && git push --tags
```

In the project's CI/CD settings, set the configuration file to
`debian/salsa-ci.yml`. Every push then builds the package and runs lintian,
autopkgtest, piuparts and reproducibility checks.

### 3. File the ITP

Edit the "Why package it" paragraph in `submission/ITP.txt` into your own
words, then send it from the address in `Maintainer:`. The bug tracker
replies with a number. Put it in `debian/changelog` in place of `#ITPBUG`.

### 4. Find a sponsor

* Build and sign the source package (`debuild -S`, or `gbp buildpackage -S`)
  and upload it to mentors.debian.net with `dput mentors`.
* Send `submission/RFS.txt`.
* Say hello on the Rust team's list (debian-rust@lists.debian.org) or Matrix
  room (#debian-rust:matrix.org) and point them at the RFS. The Rust team
  reviews Rust packages fastest.

### 5. NEW queue, testing, Ubuntu

The ftpmasters review licensing and copyright for every new package; expect
weeks to a few months. Once accepted, coal migrates to testing after about
five days if nothing breaks, and Ubuntu's devel series imports it
automatically.

### 6. The PPA, in the meantime

Create a PPA called `coal` on Launchpad (and enable arm64 in its settings if
you want it). Then, from a checkout of the release tag:

```sh
ORIG=coal-0.4.8.tar.gz packaging/ppa/build-ppa-source.sh noble
ORIG=coal-0.4.8.tar.gz packaging/ppa/build-ppa-source.sh resolute
debsign target/ppa/*/coal_*_source.changes
dput ppa:<your-launchpad-id>/coal target/ppa/noble/coal_*_source.changes
dput ppa:<your-launchpad-id>/coal target/ppa/resolute/coal_*_source.changes
```

Pass the GitHub tag tarball as `ORIG` so the PPA and Debian use the same
upstream tarball. Without it the script uses `git archive HEAD`. Users then
install with:

```sh
sudo add-apt-repository ppa:<your-launchpad-id>/coal
sudo apt install coal coal-lsp
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
* `install_private.sh` and the README's install line assume a private
  repository. Once the repository is public, `install_private.sh` can go.
