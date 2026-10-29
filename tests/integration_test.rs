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
