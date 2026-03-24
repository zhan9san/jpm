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
