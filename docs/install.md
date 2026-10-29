# jpm install — Design Document

## Command

```text
jpm install [OPTIONS]

Options:
  -l, --lock <FILE>        Lock file to install from  [default: plugins-lock.txt]
  -d, --plugin-dir <DIR>   Target plugin directory    [default: ./plugins/]
      --skip-failed        Warn on failures instead of aborting
      --dry-run            Print what would be installed without downloading
```

---

## Overview

`jpm install` reads a `plugins-lock.txt` file and downloads every pinned plugin
into a plugin directory. It is the install-time counterpart to `jpm lock`.

```text
plugins-lock.txt
      │
      ▼
 parse entries (name, version, sha256)
      │
      ▼
 scan existing .hpi / .jpi files in --plugin-dir
      │
      ▼
 filter: skip plugins already at >= required version
      │
      ▼
 download all remaining plugins concurrently (tokio tasks)
      │  ├─ primary:   https://updates.jenkins.io/download/plugins/…
      │  └─ fallback:  https://archives.jenkins.io/plugins/…
      │
      ▼
 verify sha256 of each downloaded file
      │
      ▼
 atomic move: tmp file → plugin-dir/<name>.hpi
```

---

## Step-by-Step Logic

### 1. Parse the lock file

Read `plugins-lock.txt` using the existing `lockfile::parse()` function.
Each line yields:

```text
git:5.7.0 sha256:IPcSG3z9odMbDER7PjWY3J5fBP1f5+nnhBIqSWwuXOo=
  ↑   ↑              ↑
name version     base64-encoded sha256 (as published by the UC)
```

Abort if the lock file is missing. A missing lock means `jpm lock` has not
been run — installing from an unknown set of versions would not be
reproducible.

### 2. Scan existing plugins

Walk `--plugin-dir` and collect all `.hpi` and `.jpi` files. For each file,
read its `META-INF/MANIFEST.MF` (the `.hpi` is a ZIP) and extract:

```text
Plugin-Version: 5.7.0
Short-Name: git
```

Build a map of `name → installed_version` from the scan results.

### 3. Determine what to download

For each entry in the lock file:

```text
if installed_version >= locked_version → skip (already satisfied)
else                                   → queue for download
```

Version comparison uses the same `JenkinsVersion` comparator from
`src/version.rs`.

Print a summary before downloading:

```text
  12 plugin(s) already up to date
  41 plugin(s) to download
```

### 4. Download concurrently

Spawn one `tokio` task per plugin that needs downloading. A semaphore caps
how many downloads run at once (`--concurrency`, default 8). Each task:

1. Build the primary download URL:

   ```text
   https://updates.jenkins.io/download/plugins/<name>/<version>/<name>.hpi
   ```

   Respect the `JENKINS_UC_DOWNLOAD_URL` and (deprecated)
   `JENKINS_UC_DOWNLOAD` environment variables as overrides (mirrors the Java
   tool's behaviour, see [Download URL Construction](#download-url-construction)).

2. Stream the response to `<plugin-dir>/.jpm-staging-<random>/<name>.jpi`,
   computing the SHA-256 while writing, so archives are never held in memory
   and Jenkins never sees a partial `.jpi`.

3. On network error or non-2xx status: retry up to **3 times** with a brief
   back-off, then fall back to the archives mirror:

   ```text
   https://archives.jenkins.io/plugins/<name>/<version>/<name>.hpi
   ```

4. If both primary and fallback fail: record as a failure.

### 5. Verify sha256

After each successful download, compute the SHA-256 of the temp file and
compare it against the value stored in the lock file.

The UC publishes checksums as **Base64-encoded** strings (standard encoding,
not URL-safe). Verification:

```text
sha256(downloaded_bytes) → Base64::encode → compare to lock file value
```

On mismatch: delete the temp file and record a failure. Do **not** move a
file with a bad checksum into the plugin directory.

If the lock file entry has no `sha256` (e.g. a bundled-only plugin), skip
verification and warn.

Then open the file as a ZIP and read `META-INF/MANIFEST.MF`. The download is
rejected unless it is a valid archive whose `Short-Name` (when present) and
`Plugin-Version` match the lock entry. This catches a mirror answering with
an HTML error page even when there is no checksum to compare.

### 6. Atomic move to plugin directory

Only after **every** download is verified (or with `--skip-failed`, after
all downloads finished) move the staged files to their final location:

```text
<plugin-dir>/.jpm-staging-<random>/<name>.jpi  →  <plugin-dir>/<name>.jpi
```

If any download fails without `--skip-failed`, the staging directory is
removed and the plugin directory is left exactly as it was.

Use `std::fs::rename` (atomic on the same filesystem). If `rename` fails
(cross-device move), fall back to copy + delete.

If `<plugin-dir>/<name>.jpi` already exists, it is first moved to
`<plugin-dir>/<name>.bak` (replacing an older backup). Jenkins offers that
file as "Downgrade to …" in its plugin manager.

### 7. Error handling

| Mode | Behaviour on failure |
|---|---|
| Default (strict) | Abort after all downloads complete; report all failures at once; plugin directory unchanged |
| `--skip-failed` | Warn, install the plugins that succeeded; exit code 0 |

Always print a final summary:

```text
installed 41 plugin(s)
failed    2 plugin(s): pipeline-model-definition, workflow-cps
```

---

## Download URL Construction

Priority order (mirrors the Java plugin manager):

| Condition | URL used |
|---|---|
| Lock entry has `url:<url>` | `<url>` (`file://` is copied; no archives fallback) |
| `JENKINS_UC_DOWNLOAD_URL` env var is set | `$JENKINS_UC_DOWNLOAD_URL/<n>/<v>/<n>.hpi` |
| `JENKINS_UC_DOWNLOAD` env var is set (deprecated) | `$JENKINS_UC_DOWNLOAD/plugins/<n>/<v>/<n>.hpi` |
| `JENKINS_UC` env var is set | `<scheme://host:port of JENKINS_UC>/download/plugins/<n>/<v>/<n>.hpi` |
| Standard case | `https://updates.jenkins.io/download/plugins/<n>/<v>/<n>.hpi` |
| Mirror fails | `https://archives.jenkins.io/plugins/<n>/<v>/<n>.hpi` |

---

## SHA-256 Encoding Note

The Jenkins Update Center stores checksums as **standard Base64** (with `=`
padding), not hex. Example from `plugin-versions.json`:

```json
"sha256": "IPcSG3z9odMbDER7PjWY3J5fBP1f5+nnhBIqSWwuXOo="
```

This is stored verbatim in `plugins-lock.txt`. During verification, compute
the SHA-256 of the downloaded bytes, Base64-encode the result, and compare
strings directly.

---

## Module Structure

```text
src/
├── installer.rs        ← new module
│   ├── scan_installed()        scan plugin-dir for existing .hpi/.jpi files
│   ├── read_manifest()         unzip .hpi, parse META-INF/MANIFEST.MF
│   ├── download_plugin()       single-plugin download + retry + fallback
│   ├── verify_sha256()         compare downloaded bytes against lock entry
│   └── install()               top-level entry point, orchestrates all steps
└── main.rs             ← add `install` subcommand via clap subcommands
```

---

## Comparison with Java Tool

| Behaviour | Java tool | jpm install |
|---|---|---|
| Concurrency | 64-thread `ForkJoinPool` | `tokio` tasks (one per plugin) |
| Skip check | re-reads MANIFEST.MF from disk | re-reads MANIFEST.MF from disk |
| Checksum source | Fetches UC JSON at runtime | Already in lock file — no extra request |
| Fallback URL | `archives.jenkins.io` | `archives.jenkins.io` |
| Retries | 3 (configurable) | 3 |
| Atomic write | `Files.move` + `REPLACE_EXISTING` | `fs::rename` + copy fallback |
| Skip-failed flag | `--skip-failed-plugins` | `--skip-failed` |
| Credential support | Basic auth via `--credentials` | Not in scope for v1 |

**Key advantage of `jpm install` over the Java tool:** because `sha256` is
already stored in the lock file, no Update Center request is needed at install
time. The Java tool must re-fetch and re-parse the full UC JSON on every
install run.

---

## Out of Scope for v1

- HTTP Basic Auth / credentials (the Java tool supports `--credentials`)
- Incremental releases (`incrementals;groupId;version` syntax)
- Installing from a custom URL override (the `::url` column in `plugins.txt`)
- Progress bars / download speed reporting
