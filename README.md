# conch
Conch Shell

## Install

**Cargo**

```sh
cargo install conch
```

**Homebrew** (macOS/Linux)

```sh
brew tap prymul/conch
brew install conch
```

**Debian/Ubuntu (.deb)**

Download the `.deb` from the [latest release](https://github.com/prymul/conch/releases/latest) and:

```sh
sudo dpkg -i conch_*.deb
```

## Releases

Versioning is automated with [cocogitto](https://docs.cocogitto.io/) based on
[Conventional Commits](https://www.conventionalcommits.org/). Merging
conventional commits into `main` triggers an automatic semver bump, tag, and
GitHub release, which in turn publishes to crates.io, updates the Homebrew
tap, and attaches a `.deb` package to the release.

Local commits are checked against the Conventional Commits spec with
[cocogitto](https://docs.cocogitto.io/), scanned for secrets with
[gitleaks](https://github.com/gitleaks/gitleaks), and linted/tested with
`cargo fmt`/`clippy`/`test` — the same checks CI runs. These all run via git
hooks defined in `lefthook.yml` and installed once per clone with:

```sh
lefthook install
```

Dependencies are audited against the [RustSec advisory
database](https://rustsec.org/) in CI via `cargo-audit`.
