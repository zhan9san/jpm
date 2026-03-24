# jpm Graph Design

## Goal

Add graph output to help users understand dependency edges and cycles,
without changing the lock resolution rules.

## Command shape

Primary command:

- `jpm graph`

Input options (exactly one required):

- `-f, --file <plugins.txt>`
- `-l, --lock <plugins-lock.txt>`

Optional compatibility alias (future):

- `jpm lock --graph ...`

## Logic source

`jpm graph` reuses `jpm lock` graph construction logic (UC deps, detached
split implied edges, split cycle-break removals).

### `-f` input policy

For `-f` input, graph uses the same manifest-driven transitive resolution
policy as `jpm lock` (no bundled-version uplift during resolve).

Concretely:

- `jpm lock` preserves manifest-driven transitive versions.
- `jpm graph -f` preserves manifest-driven transitive versions.

Reason:

- split-edge injection depends on effective plugin versions
- effective plugin versions drive `requiredCore <= splitWhen` checks
- bundled uplift can change those checks and hide/show split-implied edges

This change fixes the observed case where Jenkins runtime reported a cycle that
`jpm graph -f` did not show (`caffeine-api -> sshd` edge missing before).

### `-l` input policy

For `-l` input, graph reads versions directly from lock entries and builds
edges from that pinned set.

### Shared graph pipeline

- update center dependency resolution
- bundled plugin context from WAR `pom.xml`
- split/detached implied edges
- split break-cycle edge removals

No duplicate graph-building logic should be introduced.

## Cycle behavior

If a cycle exists when `jpm graph` runs:

- still write graph output file
- highlight cycle nodes/edges in graph metadata
- print cycle summary to stderr
- exit non-zero (`1`) by default

Optional flag:

- `--allow-cycle` to return zero while still marking cycles

## Output format

Initial format:

- DOT (`.dot`) for Graphviz rendering

Future format:

- Mermaid text export

## Why this design

- keeps one shared graph model and avoids code divergence
- preserves runtime-like transitive behavior for lock and `-f` cycle analysis
- provides CI-safe failure signaling
- still gives users a visual artifact for debugging
