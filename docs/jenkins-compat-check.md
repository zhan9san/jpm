# Jenkins Version Compatibility Check

## Problem

Every plugin declares `requiredCore` — the minimum Jenkins version it needs.
If `requiredCore > target Jenkins`, the plugin fails to load at runtime with no
install-time warning. `jpm lock` must catch this at resolve time.

Data already available in `plugin-versions.json` (no extra HTTP requests):

| Plugin version | `requiredCore` |
|---|---|
| `git:5.9.0` | `2.504.3` |
| `git:5.5.0` | `2.440.3` |
| `git:5.2.0` | `2.387.3` |
| `git:4.9.0` | `2.289.1` |

---

## Flags

### `--fix` — repair broken plugins after a Jenkins upgrade

Rewrites **all incompatible plugins** in `plugins.txt` (both explicit pins and
`latest` entries) to the **highest compatible version**, then re-resolves.

```text
$ jpm lock -j 2.452.4 --fix
  fixed: git:5.9.0          → git:5.5.0   (requiredCore 2.440.3 ≤ 2.452.4)
  fixed: credentials:1371.0 → credentials:1453.v9b_a_29777a_b_fd
  fixed: mailer:472.vf7c289a → mailer:463.vedf8358e006b
  3 plugin(s) updated in plugins.txt — writing plugins-lock.txt
```

Without `--fix`, the same situation is a hard error:

```text
$ jpm lock -j 2.452.4
error: git:5.9.0 requires Jenkins >= 2.504.3 but target is 2.452.4
       → run with --fix to auto-correct plugins.txt
```

### `--upgrade` — refresh all pins to the latest compatible version

Upgrades every plugin (not just broken ones) to the highest version the target
Jenkins can load. Use when Jenkins core is unchanged but you want fresher
plugins.

```text
$ jpm lock -j 2.452.4 --upgrade
  upgraded: git:5.2.0 → git:5.5.0   (newer, still ≤ requiredCore 2.452.4)
  upgraded: credentials:1371.0 → credentials:1453.v9b_a_29777a_b_fd
```

### Combined: annual Jenkins + plugin upgrade

```bash
jpm lock -j <new_jenkins_version> --fix --upgrade
```

`--fix` handles incompatible plugins; `--upgrade` takes everything else to
the best available version. Together: *give me the best plugin set for this
Jenkins version.*

### Flag summary

| Flag | Touches | When to use |
|---|---|---|
| *(none)* | Nothing — error on incompatible | Default; safe for CI |
| `--fix` | Incompatible plugins only | After Jenkins core upgrade |
| `--upgrade` | All plugins | Periodic refresh, same Jenkins |
| `--fix --upgrade` | All plugins | Annual Jenkins + plugin upgrade |

### Version selection: why highest compatible?

| Strategy | Problem |
|---|---|
| Latest | May require Jenkins upgrade |
| Minimum | Unnecessarily old, misses bug fixes |
| **Highest compatible** | Newest version your Jenkins can actually load |

---

## Pin Override Warning

When a pinned version is silently bumped by a transitive dependency, warn
(does not block lock file generation):

```text
warning: git pinned to 4.0.0 in plugins.txt but upgraded to 5.2.0
         required by workflow-aggregator:596.v8c21c963d92d
```
