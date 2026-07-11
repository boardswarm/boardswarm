use std::process::Command;

/// Determine the version string to embed into the binary.
/// The version is exposed via the CLI argument `--version`.
///
/// Version resolution order:
/// 1. `BOARDSWARM_VERSION` environment variable
/// 2. `git describe`
/// 3. The crate version from `Cargo.toml`
fn main() {
    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap();

    let version = std::env::var("BOARDSWARM_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| git_describe().map(|git| format!("{pkg_version} ({git})")))
        .unwrap_or(pkg_version);

    println!("cargo:rustc-env=BOARDSWARM_VERSION={version}");

    // Re run when the injected version or the git state changes.
    println!("cargo:rerun-if-env-changed=BOARDSWARM_VERSION");
    for path in ["../.git/HEAD", "../.git/index"] {
        if std::path::Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

fn git_describe() -> Option<String> {
    // Ignore git tags (tags are created per crate, e.g. `boardswarm-v0.0.1`).
    let output = Command::new("git")
        .args(["describe", "--always", "--dirty", "--exclude=*"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let version = String::from_utf8(output.stdout).ok()?;
    let version = version.trim();
    (!version.is_empty()).then(|| version.to_string())
}
