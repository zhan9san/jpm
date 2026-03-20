# Feature Status: jpm vs plugin-installation-manager-tool (Java)

Reference: `plugin-installation-manager-tool/` (Java, 1684-line `PluginManager.java`)

---

## Done in jpm

| Feature | Java flag / behaviour | jpm equivalent |
|---|---|---|
| Plugin manifest (.txt) | `--plugin-file plugins.txt` | `jpm lock -f plugins.txt` |
| Dependency resolution (BFS) | `findPluginsAndDependencies()` | `resolver::resolve()` |
| Highest-version-wins conflict | `combineDependencies()` | `JenkinsVersion` comparator |
| Skip optional dependencies | `plugin.setOptional(true)` | filtered in BFS |
| Bundled plugin detection | Parses jenkins.war (ZIP-in-ZIP) | Fetches `war/pom.xml` from GitHub |
| Skip already-installed plugins | reads `.jpi` MANIFEST.MF | `installer::scan_installed()` |
| Parallel downloads | 64-thread `ForkJoinPool` | `tokio::spawn` per plugin |
| Retry + archive fallback | 3 retries → `archives.jenkins.io` | same |
| Checksum verification | sha1/sha256/sha512 (configurable) | sha256 from lock file (no UC refetch) |
| Skip-failed mode | `--skip-failed-plugins` | `jpm install --skip-failed` |
| Dry run | `--no-download` | `jpm install --dry-run` |
| Atomic plugin write | `Files.move + REPLACE_EXISTING` | `fs::rename` + copy fallback |
| `latest` version channel | `--latest true` (default) | `VersionSpec::Latest` |
| `experimental` channel | `junit:experimental` | `VersionSpec::Experimental` |
| UC data caching | `CacheManager` (1h TTL, file-based) | `~/.cache/jpm/` (1h TTL) |
| **Lock file generation** | ❌ not in Java tool | `jpm lock` ✅ new |
| **Manifest hash / staleness** | ❌ not in Java tool | `# manifest-hash:` header ✅ new |
| **sha256 in lock file** | ❌ not in Java tool | `sha256:<base64>` per line ✅ new |
| **Permanent pom.xml cache** | ❌ re-fetches on every run | `pom-{version}.xml` (no TTL) ✅ new |

---

## Not Yet Done in jpm

| Feature | Java flag | Priority | Notes |
|---|---|---|---|
| YAML input format | `--plugin-file plugins.yaml` | Medium | Java supports `.yaml`/`.yml` in addition to `.txt` |
| Inline plugin list | `--plugins git credentials` | Low | Specify plugins directly on CLI without a file |
| Security warnings | `--view-security-warnings` | Medium | Fetches advisory data from `updates.jenkins.io/current/plugin-versions.json` |
| All security warnings | `--view-all-security-warnings` | Low | Shows warnings for every plugin in the UC |
| Available updates | `--available-updates` | Medium | Compares installed vs latest; outputs diff |
| List resolved plugins | `--list` | Low | Prints what would be installed without downloading |
| Jenkins version from WAR | `--war jenkins.war` | Low | Reads `jenkins/model/Jenkins.class` from WAR to get version |
| Jenkins version from env | `JENKINS_VERSION` env var | Low | `jpm lock -j` currently required |
| Latest-specified strategy | `--latest-specified` | Low | Transitive deps of `latest` plugins also resolve to latest |
| Clean plugin directory | `--clean-download-directory` | Low | Wipes `--plugin-dir` before installing |
| Jenkins version compat check | `checkVersionCompatibility()` | Medium | Error when a plugin requires Jenkins X but you have Y |
| Custom update center URL | `--jenkins-update-center` / `JENKINS_UC` | Low | Override the stable UC endpoint |
| Custom experimental UC URL | `--jenkins-experimental-update-center` | Low | Override the experimental UC endpoint |
| Incrementals support | `incrementals;org.group;2.19-rc` | Low | Fetches from Maven incrementals repo instead of UC |
| Custom incrementals mirror | `--jenkins-incrementals-repo-mirror` | Low | Override incrementals Maven repo URL |
| HTTP credentials | `--credentials host:user:pass` | Low | Basic auth for private mirrors |
| Configurable hash function | `JENKINS_UC_HASH_FUNCTION` env | Low | Java supports sha1/sha256/sha512; jpm is sha256 only |
| `PLUGIN_DIR` env var | `PLUGIN_DIR` env | Low | Fallback for `--plugin-dir` |
| Verbose logging | `--verbose` | Low | Debug-level output throughout |
| YAML / stdout output formats | `--output yaml\|stdout` | Low | Java can output the resolved list as YAML or a human diff |
| Update single plugin | n/a (always resolves all) | Medium | `jpm lock --update git` to re-resolve one plugin only |

---

## Architecture Differences

| Aspect | Java tool | jpm |
|---|---|---|
| Primary purpose | **Install** plugins (download is default) | **Lock** file generation + **Install** from lock |
| Lock file concept | ❌ none | ✅ `plugins-lock.txt` (reproducible by design) |
| Install source | Resolves + downloads in one step | Installs from pre-generated lock file |
| Checksum source at install time | Re-fetches UC JSON | Reads from lock file (zero extra requests) |
| Bundled plugin source | Parses 80 MB WAR (ZIP-in-ZIP) | Fetches 30 KB `war/pom.xml` from GitHub |
| Dependency strategy | `--latest` (true/false) or `--latest-specified` | highest-version-wins (no flags needed) |
| Concurrency model | 64-thread `ForkJoinPool` | unbounded `tokio::spawn` tasks |
| Runtime | JVM (requires Java) | native binary (no runtime) |
| Config caching | 1h TTL for all remote data | 1h TTL for UC JSON; permanent for versioned pom.xml |

---

## Summary

jpm covers the **core workflow** — resolve, lock, install — plus adds the lock file
concept that the Java tool lacks entirely. The missing features are mostly
informational (`--list`, `--available-updates`, security warnings) or edge-case
input formats (YAML, incrementals, inline CLI plugins).
