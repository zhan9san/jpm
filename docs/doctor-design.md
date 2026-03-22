# jpm Doctor Design

## Context

`jpm` serves two modes:

- immutable image builds (fresh plugin dir)
- in-place upgrades on long-lived Jenkins servers

The second mode is where runtime surprises happen, because Jenkins
loads what is on disk, not only what lock generation intended.

## Decision

Primary solution: add `jpm doctor`.

- command: `jpm doctor --lock plugins-lock.txt --plugin-dir plugins`
- optional: `--strict` to fail on risky findings

This keeps validation separate from mutation (`jpm install`), and
works as a CI gate before restart.

## Checks

Given lock file + plugin directory:

- `duplicate_suffix`: both `name.hpi` and `name.jpi` exist
- `version_drift`: on-disk plugin version differs from lock
- `unmanaged_plugin`: plugin exists on disk but not in lock
- `disabled_marker`: `*.disabled` exists for a plugin archive

## Output contract

Each finding includes:

- code
- severity (`warning` or `error`)
- plugin
- remediation

Codes:

- `JPM001 duplicate_suffix`
- `JPM002 version_drift`
- `JPM003 unmanaged_plugin`
- `JPM004 disabled_marker`

## Policy by operating mode

- image build: doctor optional, warning mode is usually enough
- long-lived upgrade: run doctor before install/restart, use strict mode

## Remediation playbook

Recommended flow for in-place upgrades:

1. Backup plugin directory.
2. Run `jpm doctor --strict`.
3. Fix findings by code.
4. Re-run `jpm doctor --strict` until clean.
5. Restart Jenkins and verify startup logs.

Fix guidance per code:

- `JPM001 duplicate_suffix`
  - keep one canonical archive (prefer `.jpi`)
  - remove duplicate suffix artifact
- `JPM002 version_drift`
  - lock is source of truth: apply `jpm install`
  - disk is source of truth: update manifest and regenerate lock
- `JPM003 unmanaged_plugin`
  - required plugin: add to `plugins.txt` and regenerate lock
  - unmanaged plugin: remove from plugin dir
- `JPM004 disabled_marker`
  - expected active: remove `.disabled`
  - intentionally disabled: keep marker and document exception

## Rollout

1. Implement warning mode.
2. Add strict mode and stable exit behavior.
3. Add runbook examples in `docs/install.md`.
