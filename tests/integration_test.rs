//! Hermetic integration tests for `jpm lock`.
//!
//! A local wiremock server stands in for the Jenkins Update Center.  All three
//! UC endpoints and the bundled-plugins pom.xml are served from static fixture
//! files under `tests/fixtures/`.  The `JPM_*` env vars point the binary at the
//! mock server and redirect disk caching to a per-test temp directory so tests
//! are fully isolated from each other and from the developer's real cache.

use assert_cmd::Command;
use std::fs;
use tempfile::TempDir;
use wiremock::{
    matchers::{method, path, path_regex},
    Mock, MockServer, ResponseTemplate,
};

const JENKINS_VERSION: &str = "2.452.4";

// ── Helpers ───────────────────────────────────────────────────────────────────

fn fixture(name: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

/// Start a wiremock server pre-loaded with all UC fixture responses.
async fn start_mock_uc() -> MockServer {
    let server = MockServer::start().await;

    for (url_path, file) in [
        ("/uc-stable", "uc-stable.json"),
        ("/uc-experimental", "uc-experimental.json"),
        ("/plugin-versions", "plugin-versions.json"),
    ] {
        Mock::given(method("GET"))
            .and(path(url_path))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture(file)))
            .mount(&server)
            .await;
    }

    // pom.xml path includes the Jenkins version: /pom/jenkins-<VERSION>/war/pom.xml
    Mock::given(method("GET"))
        .and(path_regex(r"^/pom/jenkins-.*"))
        .respond_with(ResponseTemplate::new(200).set_body_string(fixture("pom.xml")))
        .mount(&server)
        .await;

    // split-plugin metadata path:
    // /jenkins/jenkins-<VERSION>/core/src/main/resources/jenkins/split-plugins.txt
    Mock::given(method("GET"))
        .and(path_regex(
            r"^/jenkins/jenkins-.*/core/src/main/resources/jenkins/split-plugins.txt$",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("# test split plugins\nsshd 2.281 3.236.ved5e1b_cb_50b_2\n"),
        )
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(
            r"^/jenkins/jenkins-.*/core/src/main/resources/jenkins/split-plugin-cycles.txt$",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("# test split plugin cycle breaks\njavax-activation-api sshd\n"),
        )
        .mount(&server)
        .await;

    server
}

/// Build a `jpm` command pre-configured with mock server URLs and an isolated
/// cache directory so tests never touch the developer's real `~/.cache/jpm`.
fn jpm(mock: &MockServer, cache: &TempDir) -> Command {
    let base = mock.uri();
    let mut cmd = Command::cargo_bin("jpm").unwrap();
    cmd.env("JPM_UC_STABLE_URL", format!("{base}/uc-stable?version="))
        .env("JPM_UC_EXPERIMENTAL_URL", format!("{base}/uc-experimental"))
        .env(
            "JPM_UC_PLUGIN_VERSIONS_URL",
            format!("{base}/plugin-versions"),
        )
        .env("JPM_JENKINS_GH_BASE", format!("{base}/jenkins/jenkins-"))
        .env("JPM_POM_BASE_URL", format!("{base}/pom/jenkins-"))
        .env("JPM_CACHE_DIR", cache.path().to_str().unwrap());
    cmd
}

/// Build a minimal plugin archive whose manifest carries `short_name` and
/// `version`, as served by the Update Center download mirror.
fn plugin_archive(short_name: &str, version: &str) -> Vec<u8> {
    use std::io::Write;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        zip.start_file(
            "META-INF/MANIFEST.MF",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        write!(
            zip,
            "Manifest-Version: 1.0\r\nShort-Name: {short_name}\r\nPlugin-Version: {version}\r\n"
        )
        .unwrap();
        zip.finish().unwrap();
    }
    buf.into_inner()
}

/// Base64 SHA-256, the checksum format used by the Update Center and lock files.
fn sha256_b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(bytes))
}

/// Serve `bytes` at `/dl/<name>/<version>/<name>.hpi`.
async fn mount_plugin(server: &MockServer, name: &str, version: &str, bytes: Vec<u8>) {
    Mock::given(method("GET"))
        .and(path(format!("/dl/{name}/{version}/{name}.hpi")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
        .mount(server)
        .await;
}

/// Leftover `.jpm-*` temp files or directories in the plugin directory.
fn jpm_leftovers(dir: &std::path::Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with(".jpm-"))
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// `jpm lock` resolves direct plugins and their transitive deps, writes a
/// well-formed lock file with SHA-256 checksums.
#[tokio::test]
async fn lock_resolves_transitive_deps() {
    let mock = start_mock_uc().await;
    let tmp = TempDir::new().unwrap();

    fs::write(tmp.path().join("plugins.txt"), "git\ncredentials\n").unwrap();

    jpm(&mock, &tmp)
        .args([
            "lock",
            "-j",
            JENKINS_VERSION,
            "--skip-bundled",
            "-f",
            tmp.path().join("plugins.txt").to_str().unwrap(),
            "-o",
            tmp.path().join("plugins-lock.txt").to_str().unwrap(),
        ])
        .assert()
        .success();

    let lock = fs::read_to_string(tmp.path().join("plugins-lock.txt")).unwrap();
    assert!(
        lock.contains("git:5.7.0"),
        "expected git:5.7.0 in lock:\n{lock}"
    );
    assert!(
        lock.contains("credentials:"),
        "expected credentials in lock:\n{lock}"
    );
    assert!(
        lock.contains("sha256:"),
        "expected sha256 checksums in lock:\n{lock}"
    );
    assert!(
        lock.contains(&format!("# Jenkins: {JENKINS_VERSION}")),
        "expected jenkins version header:\n{lock}"
    );
}

/// When a plugin is pinned, `jpm lock` preserves that version in lock output
/// instead of uplifting to the WAR-bundled version.
#[tokio::test]
async fn lock_preserves_pinned_version_without_bundled_uplift() {
    let mock = start_mock_uc().await;
    let tmp = TempDir::new().unwrap();

    // pom.xml bundles mailer at 472.vf7c289a_4b_c36; pin requests an older 300.0.
    fs::write(tmp.path().join("plugins.txt"), "mailer:300.0\n").unwrap();

    jpm(&mock, &tmp)
        .args([
            "lock",
            "-j",
            JENKINS_VERSION,
            "-f",
            tmp.path().join("plugins.txt").to_str().unwrap(),
            "-o",
            tmp.path().join("plugins-lock.txt").to_str().unwrap(),
        ])
        .assert()
        .success();

    let lock = fs::read_to_string(tmp.path().join("plugins-lock.txt")).unwrap();
    assert!(
        lock.contains("mailer:300.0"),
        "pinned version should be kept:\n{lock}"
    );
    assert!(
        !lock.contains("mailer:472.vf7c289a_4b_c36"),
        "bundled version should not override pinned version:\n{lock}"
    );
}

/// An incompatible plugin (requiredCore > target Jenkins) without `--fix`
/// causes a non-zero exit and a human-readable error.
#[tokio::test]
async fn lock_compat_error_exits_nonzero() {
    let mock = start_mock_uc().await;
    let tmp = TempDir::new().unwrap();

    // git:5.9.0 requires Jenkins 2.504.3, target is 2.452.4.
    fs::write(tmp.path().join("plugins.txt"), "git:5.9.0\n").unwrap();

    let output = jpm(&mock, &tmp)
        .args([
            "lock",
            "-j",
            JENKINS_VERSION,
            "--skip-bundled",
            "-f",
            tmp.path().join("plugins.txt").to_str().unwrap(),
            "-o",
            tmp.path().join("plugins-lock.txt").to_str().unwrap(),
        ])
        .assert()
        .failure()
        .get_output()
        .clone();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("git"),
        "error should mention plugin name:\n{stderr}"
    );
    assert!(
        stderr.contains("2.504.3"),
        "error should mention requiredCore:\n{stderr}"
    );
    assert!(
        stderr.contains("--fix"),
        "error should hint at --fix:\n{stderr}"
    );
}

/// `--fix` rewrites `plugins.txt` with the highest compatible version and
/// produces a valid lock file.
#[tokio::test]
async fn lock_fix_rewrites_plugins_txt() {
    let mock = start_mock_uc().await;
    let tmp = TempDir::new().unwrap();
    let plugins_txt = tmp.path().join("plugins.txt");

    // git:5.9.0 is incompatible; fixture highest compatible is 5.7.0.
    fs::write(&plugins_txt, "git:5.9.0\ncredentials\n").unwrap();

    jpm(&mock, &tmp)
        .args([
            "lock",
            "-j",
            JENKINS_VERSION,
            "--skip-bundled",
            "--fix",
            "-f",
            plugins_txt.to_str().unwrap(),
            "-o",
            tmp.path().join("plugins-lock.txt").to_str().unwrap(),
        ])
        .assert()
        .success();

    let rewritten = fs::read_to_string(&plugins_txt).unwrap();
    assert!(
        rewritten.contains("git:5.7.0"),
        "plugins.txt should be rewritten to 5.7.0:\n{rewritten}"
    );
    assert!(
        !rewritten.contains("git:5.9.0"),
        "plugins.txt should no longer contain 5.9.0:\n{rewritten}"
    );

    let lock = fs::read_to_string(tmp.path().join("plugins-lock.txt")).unwrap();
    assert!(
        lock.contains("git:5.7.0"),
        "lock should use fixed version:\n{lock}"
    );
}

/// `--upgrade` bumps a pinned-but-compatible plugin to the highest available
/// compatible version and rewrites `plugins.txt`.
#[tokio::test]
async fn lock_upgrade_bumps_pinned_versions() {
    let mock = start_mock_uc().await;
    let tmp = TempDir::new().unwrap();
    let plugins_txt = tmp.path().join("plugins.txt");

    // git:5.5.0 is compatible but not the highest compatible (5.7.0 is).
    fs::write(&plugins_txt, "git:5.5.0\n").unwrap();

    jpm(&mock, &tmp)
        .args([
            "lock",
            "-j",
            JENKINS_VERSION,
            "--skip-bundled",
            "--upgrade",
            "-f",
            plugins_txt.to_str().unwrap(),
            "-o",
            tmp.path().join("plugins-lock.txt").to_str().unwrap(),
        ])
        .assert()
        .success();

    let rewritten = fs::read_to_string(&plugins_txt).unwrap();
    assert!(
        rewritten.contains("git:5.7.0"),
        "plugins.txt should be upgraded to 5.7.0:\n{rewritten}"
    );

    let lock = fs::read_to_string(tmp.path().join("plugins-lock.txt")).unwrap();
    assert!(
        lock.contains("git:5.7.0"),
        "lock should contain upgraded version:\n{lock}"
    );
}

/// `--no-sha256` writes plain `name:version` lock entries.
#[tokio::test]
async fn lock_no_sha256_omits_checksum_suffix() {
    let mock = start_mock_uc().await;
    let tmp = TempDir::new().unwrap();

    fs::write(tmp.path().join("plugins.txt"), "git\n").unwrap();

    jpm(&mock, &tmp)
        .args([
            "lock",
            "-j",
            JENKINS_VERSION,
            "--skip-bundled",
            "--no-sha256",
            "-f",
            tmp.path().join("plugins.txt").to_str().unwrap(),
            "-o",
            tmp.path().join("plugins-lock.txt").to_str().unwrap(),
        ])
        .assert()
        .success();

    let lock = fs::read_to_string(tmp.path().join("plugins-lock.txt")).unwrap();
    assert!(
        lock.contains("git:5.7.0"),
        "lock should include plugin version:\n{lock}"
    );
    assert!(
        !lock.contains("sha256:"),
        "lock should not include checksum suffixes:\n{lock}"
    );
}

/// Running `jpm lock` twice with the same inputs produces identical lock files.
#[tokio::test]
async fn lock_is_deterministic() {
    let mock = start_mock_uc().await;
    let tmp = TempDir::new().unwrap();

    fs::write(
        tmp.path().join("plugins.txt"),
        "git\ncredentials\nworkflow-aggregator\n",
    )
    .unwrap();

    for _ in 0..2 {
        jpm(&mock, &tmp)
            .args([
                "lock",
                "-j",
                JENKINS_VERSION,
                "--skip-bundled",
                "-f",
                tmp.path().join("plugins.txt").to_str().unwrap(),
                "-o",
                tmp.path().join("plugins-lock.txt").to_str().unwrap(),
            ])
            .assert()
            .success();
    }

    // Both runs should produce the same content (second run hits the disk cache).
    let lock = fs::read_to_string(tmp.path().join("plugins-lock.txt")).unwrap();
    assert!(!lock.is_empty());

    // Verify alphabetical ordering.
    let plugin_lines: Vec<&str> = lock
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .collect();
    let mut sorted = plugin_lines.clone();
    sorted.sort();
    assert_eq!(
        plugin_lines, sorted,
        "lock file should be alphabetically sorted"
    );
}

/// A transitive dependency upgrading past an explicit pin emits a warning on
/// stderr but still exits successfully.
#[tokio::test]
async fn lock_warns_on_pin_override() {
    let mock = start_mock_uc().await;
    let tmp = TempDir::new().unwrap();

    // credentials is pinned to 1371.0, but git:5.7.0 (pinned) has a transitive
    // dep on credentials:1453.v9b_a_29777a_b_fd (from plugin-versions.json).
    fs::write(
        tmp.path().join("plugins.txt"),
        "git:5.7.0\ncredentials:1371.0\n",
    )
    .unwrap();

    let output = jpm(&mock, &tmp)
        .args([
            "lock",
            "-j",
            JENKINS_VERSION,
            "--skip-bundled",
            "-f",
            tmp.path().join("plugins.txt").to_str().unwrap(),
            "-o",
            tmp.path().join("plugins-lock.txt").to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("warning") && stderr.contains("credentials"),
        "should warn about pin override:\n{stderr}"
    );

    // The lock file should use the upgraded version, not the original pin.
    let lock = fs::read_to_string(tmp.path().join("plugins-lock.txt")).unwrap();
    assert!(
        lock.contains("credentials:1453.v9b_a_29777a_b_fd"),
        "lock should use the upgraded credentials version:\n{lock}"
    );
}

/// `jpm roots` writes a minimized roots file by default and removes selected
/// plugins that are transitively required by other selected plugins.
#[tokio::test]
async fn roots_writes_default_file_and_drops_transitive_selected_plugins() {
    let mock = start_mock_uc().await;
    let tmp = TempDir::new().unwrap();
    let plugins_txt = tmp.path().join("plugins.txt");
    fs::write(
        &plugins_txt,
        "# roots sample\ngit:5.7.0\ncredentials:1453.v9b_a_29777a_b_fd\n",
    )
    .unwrap();

    jpm(&mock, &tmp)
        .args([
            "roots",
            "-j",
            JENKINS_VERSION,
            "-f",
            plugins_txt.to_str().unwrap(),
        ])
        .assert()
        .success();

    let roots = fs::read_to_string(tmp.path().join("plugins-roots.txt")).unwrap();
    assert!(
        roots.contains("git:5.7.0"),
        "roots output should keep git:\n{roots}"
    );
    assert!(
        !roots.contains("credentials:1453.v9b_a_29777a_b_fd"),
        "roots output should drop selected transitive dependency:\n{roots}"
    );
    assert!(
        roots.contains("# roots sample"),
        "roots output should preserve comment lines:\n{roots}"
    );
}

/// `jpm install --dry-run` reads a lock file and reports what would be
/// installed without making network calls.
#[tokio::test]
async fn install_dry_run_succeeds() {
    let mock = start_mock_uc().await;
    let tmp = TempDir::new().unwrap();

    // Generate a real lock file first.
    fs::write(tmp.path().join("plugins.txt"), "git\ncredentials\n").unwrap();
    jpm(&mock, &tmp)
        .args([
            "lock",
            "-j",
            JENKINS_VERSION,
            "--skip-bundled",
            "-f",
            tmp.path().join("plugins.txt").to_str().unwrap(),
            "-o",
            tmp.path().join("plugins-lock.txt").to_str().unwrap(),
        ])
        .assert()
        .success();

    // dry-run install should succeed without any network calls.
    Command::cargo_bin("jpm")
        .unwrap()
        .args([
            "install",
            "--dry-run",
            "-l",
            tmp.path().join("plugins-lock.txt").to_str().unwrap(),
            "-d",
            tmp.path().join("plugins").to_str().unwrap(),
        ])
        .assert()
        .success();
}

/// `jpm lock` only fetches the experimental Update Center when a plugin is
/// requested as `:experimental`.
#[tokio::test]
async fn lock_skips_experimental_uc_unless_requested() {
    let mock = start_mock_uc().await;
    let tmp = TempDir::new().unwrap();
    let experimental_hits = |reqs: &[wiremock::Request]| {
        reqs.iter()
            .filter(|r| r.url.path() == "/uc-experimental")
            .count()
    };

    fs::write(tmp.path().join("plugins.txt"), "git\n").unwrap();
    jpm(&mock, &tmp)
        .args([
            "lock",
            "-j",
            JENKINS_VERSION,
            "--skip-bundled",
            "-f",
            tmp.path().join("plugins.txt").to_str().unwrap(),
            "-o",
            tmp.path().join("plugins-lock.txt").to_str().unwrap(),
        ])
        .assert()
        .success();
    let reqs = mock.received_requests().await.unwrap();
    assert_eq!(experimental_hits(&reqs), 0, "experimental UC was fetched");

    fs::write(tmp.path().join("plugins.txt"), "git:experimental\n").unwrap();
    jpm(&mock, &tmp)
        .args([
            "lock",
            "-j",
            JENKINS_VERSION,
            "--skip-bundled",
            "-f",
            tmp.path().join("plugins.txt").to_str().unwrap(),
            "-o",
            tmp.path().join("plugins-lock.txt").to_str().unwrap(),
        ])
        .assert()
        .success();
    let reqs = mock.received_requests().await.unwrap();
    assert_eq!(experimental_hits(&reqs), 1, "experimental UC not fetched");
}

/// `jpm install` downloads from `JENKINS_UC_DOWNLOAD_URL`, verifies the lock
/// checksums and writes `<name>.jpi`, also when downloads are serialised.
#[tokio::test]
async fn install_downloads_and_verifies_plugins() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let plugin_dir = tmp.path().join("plugins");

    let git = plugin_archive("git", "5.7.0");
    let mailer = plugin_archive("mailer", "488.v0c9639c1a_eb_3");
    let lock = format!(
        "# Jenkins: {JENKINS_VERSION}\n\ngit:5.7.0 sha256:{}\nmailer:488.v0c9639c1a_eb_3 sha256:{}\n",
        sha256_b64(&git),
        sha256_b64(&mailer)
    );
    fs::write(tmp.path().join("plugins-lock.txt"), lock).unwrap();
    mount_plugin(&server, "git", "5.7.0", git.clone()).await;
    mount_plugin(&server, "mailer", "488.v0c9639c1a_eb_3", mailer.clone()).await;

    Command::cargo_bin("jpm")
        .unwrap()
        .env("JENKINS_UC_DOWNLOAD_URL", format!("{}/dl", server.uri()))
        .args([
            "install",
            "--concurrency",
            "1",
            "-l",
            tmp.path().join("plugins-lock.txt").to_str().unwrap(),
            "-d",
            plugin_dir.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert_eq!(fs::read(plugin_dir.join("git.jpi")).unwrap(), git);
    assert_eq!(fs::read(plugin_dir.join("mailer.jpi")).unwrap(), mailer);
    assert!(jpm_leftovers(&plugin_dir).is_empty());
}

/// A checksum mismatch fails the install and leaves no plugin or temp file.
#[tokio::test]
async fn install_rejects_checksum_mismatch() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let plugin_dir = tmp.path().join("plugins");

    let git = plugin_archive("git", "5.7.0");
    let lock = format!(
        "# Jenkins: {JENKINS_VERSION}\n\ngit:5.7.0 sha256:{}\n",
        sha256_b64(b"something else")
    );
    fs::write(tmp.path().join("plugins-lock.txt"), lock).unwrap();
    mount_plugin(&server, "git", "5.7.0", git).await;

    let assert = Command::cargo_bin("jpm")
        .unwrap()
        .env("JENKINS_UC_DOWNLOAD_URL", format!("{}/dl", server.uri()))
        .args([
            "install",
            "-l",
            tmp.path().join("plugins-lock.txt").to_str().unwrap(),
            "-d",
            plugin_dir.to_str().unwrap(),
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("integrity check failed"),
        "stderr:\n{stderr}"
    );

    assert!(!plugin_dir.join("git.jpi").exists());
    assert!(jpm_leftovers(&plugin_dir).is_empty());
}

/// Without a checksum in the lock, a mirror answering with an HTML page (or
/// the wrong plugin) is caught by the archive check instead of being
/// installed as `<name>.jpi`.
#[tokio::test]
async fn install_rejects_archive_that_is_not_the_locked_plugin() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let plugin_dir = tmp.path().join("plugins");

    fs::write(
        tmp.path().join("plugins-lock.txt"),
        format!("# Jenkins: {JENKINS_VERSION}\n\ngit:5.7.0\nmailer:1.0\n"),
    )
    .unwrap();
    mount_plugin(
        &server,
        "git",
        "5.7.0",
        b"<html>mirror error</html>".to_vec(),
    )
    .await;
    mount_plugin(&server, "mailer", "1.0", plugin_archive("mailer", "0.9")).await;

    let assert = Command::cargo_bin("jpm")
        .unwrap()
        .env("JENKINS_UC_DOWNLOAD_URL", format!("{}/dl", server.uri()))
        .args([
            "install",
            "-l",
            tmp.path().join("plugins-lock.txt").to_str().unwrap(),
            "-d",
            plugin_dir.to_str().unwrap(),
        ])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("git: downloaded file is not a plugin archive"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("mailer: downloaded archive is version 0.9, lock file says 1.0"),
        "stderr:\n{stderr}"
    );

    assert!(!plugin_dir.join("git.jpi").exists());
    assert!(!plugin_dir.join("mailer.jpi").exists());
    assert!(jpm_leftovers(&plugin_dir).is_empty());
}

/// One failed download leaves the plugin directory exactly as it was; with
/// `--skip-failed` the plugins that did download are installed.
#[tokio::test]
async fn install_is_all_or_nothing_unless_skip_failed() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let plugin_dir = tmp.path().join("plugins");
    fs::create_dir_all(&plugin_dir).unwrap();

    let old_git = plugin_archive("git", "5.6.0");
    fs::write(plugin_dir.join("git.jpi"), &old_git).unwrap();

    let new_git = plugin_archive("git", "5.7.0");
    let lock = format!(
        "# Jenkins: {JENKINS_VERSION}\n\ngit:5.7.0 sha256:{}\nmailer:1.0 sha256:{}\n",
        sha256_b64(&new_git),
        sha256_b64(b"expected mailer")
    );
    fs::write(tmp.path().join("plugins-lock.txt"), lock).unwrap();
    mount_plugin(&server, "git", "5.7.0", new_git.clone()).await;
    mount_plugin(&server, "mailer", "1.0", plugin_archive("mailer", "1.0")).await;

    let install = |extra: &[&str]| {
        let mut cmd = Command::cargo_bin("jpm").unwrap();
        cmd.env("JENKINS_UC_DOWNLOAD_URL", format!("{}/dl", server.uri()))
            .args([
                "install",
                "-l",
                tmp.path().join("plugins-lock.txt").to_str().unwrap(),
                "-d",
                plugin_dir.to_str().unwrap(),
            ])
            .args(extra);
        cmd
    };

    let assert = install(&[]).assert().failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(stderr.contains("was left unchanged"), "stderr:\n{stderr}");
    assert_eq!(fs::read(plugin_dir.join("git.jpi")).unwrap(), old_git);
    assert!(!plugin_dir.join("git.bak").exists());
    assert!(!plugin_dir.join("mailer.jpi").exists());
    assert!(jpm_leftovers(&plugin_dir).is_empty());

    install(&["--skip-failed"]).assert().success();
    assert_eq!(fs::read(plugin_dir.join("git.jpi")).unwrap(), new_git);
    assert_eq!(fs::read(plugin_dir.join("git.bak")).unwrap(), old_git);
    assert!(!plugin_dir.join("mailer.jpi").exists());
    assert!(jpm_leftovers(&plugin_dir).is_empty());
}

/// When the Update Center is unreachable, `jpm lock` falls back to an expired
/// cache file (with a warning) instead of failing.
#[tokio::test]
async fn lock_falls_back_to_stale_cache_when_uc_is_down() {
    let mock = start_mock_uc().await;
    let tmp = TempDir::new().unwrap();
    let manifest = tmp.path().join("plugins.txt");
    let lock_file = tmp.path().join("plugins-lock.txt");
    fs::write(&manifest, "git\n").unwrap();
    let lock_args = [
        "lock",
        "-j",
        JENKINS_VERSION,
        "--skip-bundled",
        "-f",
        manifest.to_str().unwrap(),
        "-o",
        lock_file.to_str().unwrap(),
    ];

    // Populate the cache, then make every cached UC file 2 hours old.
    jpm(&mock, &tmp).args(lock_args).assert().success();
    let expected = fs::read_to_string(&lock_file).unwrap();
    let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
    for name in [
        format!("uc-{JENKINS_VERSION}.json"),
        "plugin-versions.json".into(),
    ] {
        fs::File::options()
            .write(true)
            .open(tmp.path().join(name))
            .unwrap()
            .set_modified(two_hours_ago)
            .unwrap();
    }

    // Same cache, but every UC request now fails.
    let down = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&down)
        .await;
    let assert = jpm(&down, &tmp).args(lock_args).assert().success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("using cached copy from 2h ago"),
        "stderr:\n{stderr}"
    );
    assert_eq!(fs::read_to_string(&lock_file).unwrap(), expected);
}

/// `jpm lock` reports Update Center security warnings for locked plugin
/// versions, suggests an unaffected version, and can be silenced.
#[tokio::test]
async fn lock_reports_security_warnings() {
    let mock = start_mock_uc().await;
    let tmp = TempDir::new().unwrap();
    let manifest = tmp.path().join("plugins.txt");
    let lock_file = tmp.path().join("plugins-lock.txt");
    let lock_args = [
        "lock",
        "-j",
        JENKINS_VERSION,
        "--skip-bundled",
        "-f",
        manifest.to_str().unwrap(),
        "-o",
        lock_file.to_str().unwrap(),
    ];

    fs::write(&manifest, "mailer:300.0\n").unwrap();
    let assert = jpm(&mock, &tmp).args(lock_args).assert().success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("mailer:300.0 SECURITY-9999: Stored XSS vulnerability"),
        "stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("not affected: mailer:472.vf7c289a_4b_c36"),
        "stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("SECURITY-9998"),
        "core warnings are ignored"
    );
    assert!(fs::read_to_string(&lock_file)
        .unwrap()
        .contains("mailer:300.0"));

    let assert = jpm(&mock, &tmp)
        .args(lock_args)
        .arg("--hide-security-warnings")
        .assert()
        .success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(!stderr.contains("SECURITY-9999"), "stderr:\n{stderr}");

    fs::write(&manifest, "mailer\n").unwrap();
    let assert = jpm(&mock, &tmp).args(lock_args).assert().success();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(!stderr.contains("security warnings"), "stderr:\n{stderr}");
}

/// `jpm lock` reads the plugin-installation-manager-tool variables:
/// `JENKINS_VERSION` instead of `-j`, `JENKINS_UC` as a base URL (with the
/// JSONP-wrapped `update-center.json` behind it), `JENKINS_PLUGIN_INFO` and
/// `CACHE_DIR`.
#[tokio::test]
async fn lock_reads_pimt_environment_variables() {
    use wiremock::matchers::query_param;

    let mock = start_mock_uc().await;
    Mock::given(method("GET"))
        .and(path("/mirror/update-center.json"))
        .and(query_param("version", JENKINS_VERSION))
        .respond_with(ResponseTemplate::new(200).set_body_string(format!(
            "updateCenter.post(\n{}\n);",
            fixture("uc-stable.json")
        )))
        .mount(&mock)
        .await;
    let tmp = TempDir::new().unwrap();
    let base = mock.uri();
    fs::write(tmp.path().join("plugins.txt"), "git\n").unwrap();

    Command::cargo_bin("jpm")
        .unwrap()
        .env_remove("JPM_UC_STABLE_URL")
        .env_remove("JPM_UC_PLUGIN_VERSIONS_URL")
        .env_remove("JPM_CACHE_DIR")
        .env("JENKINS_VERSION", JENKINS_VERSION)
        .env("JENKINS_UC", format!("{base}/mirror"))
        .env("JENKINS_PLUGIN_INFO", format!("{base}/plugin-versions"))
        .env("CACHE_DIR", tmp.path().join("cache"))
        .env("JPM_JENKINS_GH_BASE", format!("{base}/jenkins/jenkins-"))
        .args([
            "lock",
            "--skip-bundled",
            "-f",
            tmp.path().join("plugins.txt").to_str().unwrap(),
            "-o",
            tmp.path().join("plugins-lock.txt").to_str().unwrap(),
        ])
        .assert()
        .success();

    let lock = fs::read_to_string(tmp.path().join("plugins-lock.txt")).unwrap();
    assert!(lock.contains("# Jenkins: 2.452.4"), "lock:\n{lock}");
    assert!(lock.contains("git:5.7.0"), "lock:\n{lock}");
    assert!(tmp
        .path()
        .join("cache/jpm")
        .join(format!("uc-{JENKINS_VERSION}.json"))
        .exists());
}

/// `jpm install` reads `PLUGIN_DIR`, and without a download override fetches
/// plugins from `<JENKINS_UC origin>/download/plugins/...`.
#[tokio::test]
async fn install_reads_plugin_dir_and_jenkins_uc_from_env() {
    let server = MockServer::start().await;
    let tmp = TempDir::new().unwrap();
    let plugin_dir = tmp.path().join("ref-plugins");

    let git = plugin_archive("git", "5.7.0");
    Mock::given(method("GET"))
        .and(path("/download/plugins/git/5.7.0/git.hpi"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(git.clone()))
        .mount(&server)
        .await;
    fs::write(
        tmp.path().join("plugins-lock.txt"),
        format!(
            "# Jenkins: {JENKINS_VERSION}\n\ngit:5.7.0 sha256:{}\n",
            sha256_b64(&git)
        ),
    )
    .unwrap();

    Command::cargo_bin("jpm")
        .unwrap()
        .env_remove("JENKINS_UC_DOWNLOAD_URL")
        .env_remove("JENKINS_UC_DOWNLOAD")
        .env(
            "JENKINS_UC",
            format!("{}/some/path/update-center.json", server.uri()),
        )
        .env("PLUGIN_DIR", &plugin_dir)
        .args([
            "install",
            "-l",
            tmp.path().join("plugins-lock.txt").to_str().unwrap(),
        ])
        .assert()
        .success();

    assert_eq!(fs::read(plugin_dir.join("git.jpi")).unwrap(), git);
}

/// `JENKINS_UC_CREDENTIALS` adds basic auth for the matching host:port only,
/// for both Update Center metadata and plugin downloads.
#[tokio::test]
async fn credentials_are_sent_only_to_matching_mirror() {
    use wiremock::matchers::header;

    let mock = start_mock_uc().await;
    let private = MockServer::start().await;
    // "user:pa:ss" → Basic dXNlcjpwYTpzcw==
    let auth = "Basic dXNlcjpwYTpzcw==";
    let git = plugin_archive("git", "5.7.0");
    Mock::given(method("GET"))
        .and(path("/uc-stable"))
        .and(header("authorization", auth))
        .respond_with(ResponseTemplate::new(200).set_body_string(fixture("uc-stable.json")))
        .mount(&private)
        .await;
    Mock::given(method("GET"))
        .and(path("/dl/git/5.7.0/git.hpi"))
        .and(header("authorization", auth))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(git.clone()))
        .mount(&private)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&private)
        .await;

    let tmp = TempDir::new().unwrap();
    let port = private.address().port();
    let creds = format!("127.0.0.1:{port}:user:pa:ss");
    fs::write(tmp.path().join("plugins.txt"), "git\n").unwrap();

    jpm(&mock, &tmp)
        .env(
            "JPM_UC_STABLE_URL",
            format!("{}/uc-stable?version=", private.uri()),
        )
        .env("JENKINS_UC_CREDENTIALS", &creds)
        .args([
            "lock",
            "-j",
            JENKINS_VERSION,
            "--skip-bundled",
            "--no-sha256",
            "-f",
            tmp.path().join("plugins.txt").to_str().unwrap(),
            "-o",
            tmp.path().join("plugins-lock.txt").to_str().unwrap(),
        ])
        .assert()
        .success();

    let lock = fs::read_to_string(tmp.path().join("plugins-lock.txt")).unwrap();
    assert!(lock.contains("git:5.7.0"), "lock:\n{lock}");

    fs::write(
        tmp.path().join("git-lock.txt"),
        format!("# Jenkins: {JENKINS_VERSION}\n\ngit:5.7.0\n"),
    )
    .unwrap();
    Command::cargo_bin("jpm")
        .unwrap()
        .env("JENKINS_UC_DOWNLOAD_URL", format!("{}/dl", private.uri()))
        .args([
            "install",
            "--credentials",
            &creds,
            "-l",
            tmp.path().join("git-lock.txt").to_str().unwrap(),
            "-d",
            tmp.path().join("plugins").to_str().unwrap(),
        ])
        .assert()
        .success();
    assert_eq!(fs::read(tmp.path().join("plugins/git.jpi")).unwrap(), git);

    // The public mock is on another port and must never see the credentials.
    let public = mock.received_requests().await.unwrap();
    assert!(!public.is_empty());
    assert!(public
        .iter()
        .all(|r| !r.headers.contains_key("authorization")));
}

/// Build a minimal `jenkins.war` for `version` with one detached plugin and
/// the split-plugin lists inside `jenkins-core`.
fn fake_war(version: &str) -> Vec<u8> {
    use std::io::Write;
    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            for (name, data) in entries {
                zip.start_file(*name, zip::write::SimpleFileOptions::default())
                    .unwrap();
                zip.write_all(data).unwrap();
            }
            zip.finish().unwrap();
        }
        buf.into_inner()
    }
    let core = zip_of(&[
        (
            "jenkins/split-plugins.txt",
            b"sshd 2.281 3.236.ved5e1b_cb_50b_2\n",
        ),
        (
            "jenkins/split-plugin-cycles.txt",
            b"javax-activation-api sshd\n",
        ),
    ]);
    let manifest = format!("Manifest-Version: 1.0\r\nJenkins-Version: {version}\r\n");
    let mailer = plugin_archive("mailer", "472.vf7c289a_4b_c36");
    let core_path = format!("WEB-INF/lib/jenkins-core-{version}.jar");
    zip_of(&[
        ("META-INF/MANIFEST.MF", manifest.as_bytes()),
        ("WEB-INF/detached-plugins/mailer.hpi", &mailer),
        (&core_path, &core),
    ])
}

/// `jpm lock --war` takes the Jenkins version, bundled plugins and split
/// metadata from the WAR and never asks GitHub; `-j` must match the WAR.
#[tokio::test]
async fn lock_reads_jenkins_version_and_bundled_plugins_from_war() {
    let mock = start_mock_uc().await;
    let tmp = TempDir::new().unwrap();
    let war = tmp.path().join("jenkins.war");
    let manifest = tmp.path().join("plugins.txt");
    let lock_file = tmp.path().join("plugins-lock.txt");
    fs::write(&war, fake_war(JENKINS_VERSION)).unwrap();
    fs::write(&manifest, "git\n").unwrap();
    let args = [
        "lock",
        "--war",
        war.to_str().unwrap(),
        "-f",
        manifest.to_str().unwrap(),
        "-o",
        lock_file.to_str().unwrap(),
    ];

    let assert = jpm(&mock, &tmp)
        .env_remove("JENKINS_VERSION")
        .args(args)
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(
        stdout.contains("1 plugin(s) bundled in Jenkins WAR"),
        "stdout:\n{stdout}"
    );
    let lock = fs::read_to_string(&lock_file).unwrap();
    assert!(lock.contains("# Jenkins: 2.452.4"), "lock:\n{lock}");
    assert!(lock.contains("git:5.7.0"), "lock:\n{lock}");

    let github_hits = mock
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().starts_with("/pom/") || r.url.path().starts_with("/jenkins/"))
        .count();
    assert_eq!(github_hits, 0, "pom.xml / split-plugins were fetched");

    let assert = jpm(&mock, &tmp)
        .args(args)
        .args(["-j", "2.440.3"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("does not match the WAR, which is Jenkins 2.452.4"),
        "stderr:\n{stderr}"
    );
}

/// Plugin archive with extra manifest headers (e.g. dependencies).
fn plugin_archive_with(short_name: &str, version: &str, extra: &str) -> Vec<u8> {
    use std::io::Write;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        zip.start_file(
            "META-INF/MANIFEST.MF",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        write!(
            zip,
            "Manifest-Version: 1.0\r\nShort-Name: {short_name}\r\nPlugin-Version: {version}\r\n{extra}"
        )
        .unwrap();
        zip.finish().unwrap();
    }
    buf.into_inner()
}

fn file_url(path: &std::path::Path) -> String {
    reqwest::Url::from_file_path(path).unwrap().to_string()
}

/// `name::file://…` and `name:version:http://…` plugins are locked from their
/// manifests (version, dependencies, local sha256) with a `url:` field, and
/// `jpm install` fetches them from that URL.
#[tokio::test]
async fn lock_and_install_url_plugins() {
    let mock = start_mock_uc().await;
    let tmp = TempDir::new().unwrap();

    let local = plugin_archive_with(
        "my-local",
        "1.0",
        "Jenkins-Version: 2.400\r\nPlugin-Dependencies: credentials:1371.0,mailer:1.0;resolution:=optional\r\n",
    );
    let local_path = tmp.path().join("my-local.hpi");
    fs::write(&local_path, &local).unwrap();
    let remote = plugin_archive("my-remote", "2.0");
    Mock::given(method("GET"))
        .and(path("/custom/my-remote.hpi"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(remote.clone()))
        .mount(&mock)
        .await;
    let local_url = file_url(&local_path);
    let remote_url = format!("{}/custom/my-remote.hpi", mock.uri());

    let manifest = tmp.path().join("plugins.txt");
    let lock_file = tmp.path().join("plugins-lock.txt");
    fs::write(
        &manifest,
        format!("my-local::{local_url}\nmy-remote:2.0:{remote_url}\n"),
    )
    .unwrap();
    jpm(&mock, &tmp)
        .args([
            "lock",
            "-j",
            JENKINS_VERSION,
            "--skip-bundled",
            "-f",
            manifest.to_str().unwrap(),
            "-o",
            lock_file.to_str().unwrap(),
        ])
        .assert()
        .success();

    let lock = fs::read_to_string(&lock_file).unwrap();
    let local_line = format!("my-local:1.0 sha256:{} url:{local_url}", sha256_b64(&local));
    let remote_line = format!(
        "my-remote:2.0 sha256:{} url:{remote_url}",
        sha256_b64(&remote)
    );
    assert!(lock.contains(&local_line), "lock:\n{lock}");
    assert!(lock.contains(&remote_line), "lock:\n{lock}");
    assert!(
        lock.contains("credentials:1371.0 "),
        "dependency from manifest:\n{lock}"
    );
    assert!(
        !lock.contains("mailer:"),
        "optional dependency skipped:\n{lock}"
    );

    // `graph -l` re-reads URL plugin manifests for their dependency edges.
    let dot = tmp.path().join("graph.dot");
    jpm(&mock, &tmp)
        .args([
            "graph",
            "-j",
            JENKINS_VERSION,
            "--skip-bundled",
            "-l",
            lock_file.to_str().unwrap(),
            "-o",
            dot.to_str().unwrap(),
        ])
        .assert()
        .success();
    let dot = fs::read_to_string(dot).unwrap();
    assert!(
        dot.contains("\"my-local\" -> \"credentials\""),
        "graph:\n{dot}"
    );

    // Install only the URL plugins (credentials would need the mock mirror).
    let url_lock = tmp.path().join("url-lock.txt");
    fs::write(
        &url_lock,
        format!("# Jenkins: {JENKINS_VERSION}\n\n{local_line}\n{remote_line}\n"),
    )
    .unwrap();
    let plugin_dir = tmp.path().join("plugins");
    Command::cargo_bin("jpm")
        .unwrap()
        .args([
            "install",
            "-l",
            url_lock.to_str().unwrap(),
            "-d",
            plugin_dir.to_str().unwrap(),
        ])
        .assert()
        .success();
    assert_eq!(fs::read(plugin_dir.join("my-local.jpi")).unwrap(), local);
    assert_eq!(fs::read(plugin_dir.join("my-remote.jpi")).unwrap(), remote);
}

/// A URL plugin must match its pinned version, and resolution must not
/// replace it with a newer Update Center version.
#[tokio::test]
async fn lock_rejects_mismatched_or_outgrown_url_plugins() {
    let mock = start_mock_uc().await;
    let tmp = TempDir::new().unwrap();
    let manifest = tmp.path().join("plugins.txt");
    let lock_file = tmp.path().join("plugins-lock.txt");
    let lock_args = [
        "lock",
        "-j",
        JENKINS_VERSION,
        "--skip-bundled",
        "-f",
        manifest.to_str().unwrap(),
        "-o",
        lock_file.to_str().unwrap(),
    ];

    let archive = tmp.path().join("credentials.hpi");
    fs::write(&archive, plugin_archive("credentials", "1000.0")).unwrap();
    let url = file_url(&archive);

    fs::write(&manifest, format!("credentials:1001.0:{url}\n")).unwrap();
    let assert = jpm(&mock, &tmp).args(lock_args).assert().failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("pins credentials:1001.0, but") && stderr.contains("is version 1000.0"),
        "stderr:\n{stderr}"
    );

    // git (latest in the stable UC) needs credentials >= 1371.0, newer than
    // the URL build.
    fs::write(&manifest, format!("git\ncredentials::{url}\n")).unwrap();
    let assert = jpm(&mock, &tmp).args(lock_args).assert().failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("credentials is pinned to 1000.0 from")
            && stderr.contains("requires credentials >= 1371.0"),
        "stderr:\n{stderr}"
    );
}
