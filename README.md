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

Local commits are checked against the Conventional Commits spec and scanned
for secrets with [gitleaks](https://github.com/gitleaks/gitleaks) via git
hooks installed by `cog install-hook --all`.
