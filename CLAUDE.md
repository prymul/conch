@.claude/rules/agent-teams.md

# conch

A Rust CLI. Published to crates.io as `conch-shell` (the name `conch` was
already taken by an unrelated project) via `[[bin]] name = "conch"` in
`Cargo.toml` — the installed command is still `conch`, only the registry
package name differs.

## Local dev setup

Run once per clone:

```sh
lefthook install
```

This installs pre-commit (fmt, gitleaks), pre-push (clippy, test, gitleaks),
and commit-msg (`cog verify`) hooks — see `lefthook.yml` for the exact
commands, which are copy-identical to what `.github/workflows/ci.yml` runs.
Don't also run `cog install-hook` — lefthook owns every `.git/hooks/*` file
here, and having both fight over the same file is how a hook silently gets
clobbered.

## Release process

Versioning is fully automated via cocogitto (`cog.toml`) and Conventional
Commits. Every push to `main` that isn't already a bump commit runs
`.github/workflows/release.yml`'s `bump` job: `cog bump --auto` computes the
next semver from commit history, patches `Cargo.toml`/`Cargo.lock`, commits,
tags, and pushes — all in the same job. The rest of the workflow (GitHub
release + changelog, cross-platform binaries, `.deb`, crates.io publish,
Homebrew tap update) runs as `needs: [bump, ...]` dependents in that same
workflow run, not as a separately-triggered one.

That single-workflow shape is load-bearing, not a style choice: GitHub
doesn't let a push authenticated with the default `GITHUB_TOKEN` trigger
*another* workflow via its own `on: push` listener. Splitting bump and
publish into two workflows connected by a tag push silently breaks the
second half unless the first uses a PAT — which is exactly the RELEASE_TOKEN
problem this repo doesn't have.

### Known limitation: branch protection can't gate this the "normal" way

`main` and `dev` only have rulesets blocking force-push/deletion, nothing
requiring PRs. This isn't an oversight — it was tested and found to be a
real GitHub limitation, not a config mistake: **there is no working ruleset
bypass_actors configuration that lets a `GITHUB_TOKEN`-authenticated push
bypass a "require pull request" rule.** `actor_type: "Integration"` with the
GitHub Actions app's real ID (15368) fails validation outright ("must be
part of the ruleset source or owner organization" — the built-in Actions
runner isn't a discoverable installed app). `actor_type: "User"` with the
`github-actions[bot]` user ID (41898282) validates but silently does not
bypass at push time — confirmed by reproducing the actual rejected push
locally. If this needs revisiting, the real options are a PAT/GitHub App
token with a genuinely discoverable bypass identity, or restructuring the
bump job to open a PR + auto-merge instead of pushing directly.

### Known limitation: gitleaks runs as a raw CLI in CI, not the Marketplace action

`gitleaks/gitleaks-action` requires a (free, but real) `GITLEAKS_LICENSE`
for org-owned repos. To avoid that dependency entirely, CI downloads and
runs the open-source `gitleaks` binary directly (same version pinned in
`lefthook.yml`, currently v8.30.1) instead of using the wrapper action.

### Known limitation: the Homebrew tap needs its own credential

`prymul/homebrew-conch` is a separate repo (Homebrew's `brew tap` resolution
requires a literal `homebrew-<name>` repo, not a folder in this one), so the
`homebrew` job needs write access to a repo outside its own `GITHUB_TOKEN`
scope. Deploy keys are disabled org-wide for `prymul`, so this uses a
fine-grained PAT (`HOMEBREW_TAP_TOKEN`) scoped to just that repo with
Contents: Read and write — the repo it's scoped to matters, a token scoped
to the wrong repo will pass unrelated read checks (public repos are
readable by anyone) and only fail once it actually tries to push.
