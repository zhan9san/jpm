# jpm Roots Design

## Goal

Add a command to minimize `plugins.txt` into a root-only manifest.

Root-only means:

- keep plugin `A` when no other selected plugin depends on `A`
- drop plugin `B` when some selected plugin depends on `B`

This helps users maintain short intent manifests while `plugins-lock.txt`
continues to pin the full transitive closure.

## Command shape

Primary command:

- `jpm roots`

Input:

- `-j, --jenkins-version <VERSION>` (required)
- `-f, --plugin-file <FILE>` (default: `plugins.txt`)

Output mode:

- default writes to `plugins-roots.txt`
- `--write` to rewrite `plugins.txt` in-place

Rules:

- if `--write` is not set, write default output file and print `wrote '<file>'`

Optional policy flags:

- `--keep <PLUGIN>` (repeatable): never drop these plugins

## Logic

1. Parse input `plugins.txt` with current parser (preserve comments/blank lines
   for `--write` path).
2. Resolve effective dependency graph for target Jenkins (same resolver policy
   as `jpm lock`/`jpm graph -f`).
3. Build selected set `S` = plugin names explicitly listed in input.
4. For each plugin `p` in `S`, if any other plugin `q` in `S` has a path
   `q -> ... -> p` in resolved graph, mark `p` removable.
5. Apply keep policies:
   - if `p` is in `--keep`, do not remove
6. Output deterministic minimized manifest (stable ordering).

## Edge behavior

- If `plugins.txt` contains unknown plugins, keep them and print warning.
- If graph has cycle among selected plugins, keep all cycle members and print
  warning (avoid destructive ambiguity).
- Version/core incompatibility behavior should mirror `jpm lock` checks:
  fail by default with actionable error text.

## UX examples

```bash
# Write default output file
jpm roots -j 2.452.4 -f plugins.txt

# Rewrite in place
jpm roots -j 2.452.4 -f plugins.txt --write

# Keep a transitive plugin intentionally
jpm roots -j 2.452.4 -f plugins.txt --keep ssh-credentials --write
```

## MVP scope

MVP includes only:

- root detection from selected plugins
- default output file (`plugins-roots.txt`) plus `--write`
- optional `--keep <PLUGIN>`

Deferred after MVP:

- `--keep-pinned` and other policy presets

## Non-goals

- This command does not install plugins.
- This command does not replace `jpm lock`.
- This command does not infer runtime disk state.
