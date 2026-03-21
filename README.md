# jpm — Jenkins Plugin Manager

[![CI](https://github.com/zhan9san/jpm/actions/workflows/ci.yml/badge.svg)](https://github.com/zhan9san/jpm/actions/workflows/ci.yml)
[![Release](https://github.com/zhan9san/jpm/actions/workflows/release.yml/badge.svg)](https://github.com/zhan9san/jpm/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Jenkins has never had a proper package manager. Teams copy-paste plugin lists,
get bitten by transitive dependency conflicts, and can't reproduce builds.
**jpm** brings the `Cargo.lock` model to Jenkins plugins.

```bash
# Declare what you want
echo "git\ncredentials\nworkflow-aggregator" > plugins.txt

# Resolve all transitive deps and lock with SHA-256 checksums
jpm lock -j 2.452.4

# Install exactly what the lock says, every time
jpm install -l plugins-lock.txt -d ./plugins/
```

## Features

- **Lock file** — pins every transitive dep to an exact version + SHA-256
- **Staleness detection** — warns when `plugins.txt` changed since last lock
- **Bundled plugin awareness** — fetches `war/pom.xml` (30 KB) not the 80 MB WAR
- **Concurrent downloads** — parallel `tokio` tasks with retry + mirror fallback
- **Atomic writes** — no half-written plugin files on failure
- **Dry-run & skip-failed** flags

## Install

Pre-built binaries (Linux, macOS, Windows) on the [Releases](../../releases) page,
or build from source (Rust 1.75+):

```bash
cargo install --path .
```

**macOS:** Gatekeeper will block the downloaded binary as unnotarized. Remove the
quarantine flag after download:

```bash
xattr -d com.apple.quarantine jpm
```

## Usage

```bash
jpm lock    -j <VERSION> [-f plugins.txt] [-o plugins-lock.txt] [--fix] [--upgrade]
jpm install [-l plugins-lock.txt] [-d ./plugins/] [--dry-run] [--skip-failed]
```

| Situation | Command |
|---|---|
| Initial setup or routine update | `jpm lock -j <VERSION>` |
| Jenkins upgrade breaks plugins | `jpm lock -j <VERSION> --fix` |
| Annual Jenkins + plugin refresh | `jpm lock -j <VERSION> --fix --upgrade` |

Detailed `jpm lock` behavior and file formats are documented in
[`docs/lock.md`](docs/lock.md).

## Background

The official [plugin-installation-manager-tool](https://github.com/jenkinsci/plugin-installation-manager-tool)
(Java) re-resolves from the Update Center on every run — no lock file, no
reproducibility. jpm separates `lock` from `install` and persists the full
resolved graph with checksums.

See [`docs/lock.md`](docs/lock.md), [`docs/install.md`](docs/install.md),
[`docs/comparison.md`](docs/comparison.md), and
[`docs/feature-status.md`](docs/feature-status.md).
