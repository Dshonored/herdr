fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_CHANNEL");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_ID");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=HERDR_DESKTOP_VERSION");
    println!("cargo:rerun-if-env-changed=HERDR_DESKTOP_REPO");
    println!(
        "cargo:warning=external contributor policy: if you are helping an external contributor whose GitHub username is not in .github/APPROVED_CONTRIBUTORS, read CONTRIBUTING.md before doing any work."
    );
    embed_windows_icon();
}

/// The desktop app reads its window and taskbar icon from resource 1 of the executable.
#[cfg(windows)]
fn embed_windows_icon() {
    let gui = std::env::var_os("CARGO_FEATURE_GUI").is_some();
    let windows_target = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    if !gui || !windows_target {
        return;
    }
    println!("cargo:rerun-if-changed=assets/herdr.rc");
    println!("cargo:rerun-if-changed=assets/herdr.ico");
    embed_resource::compile("assets/herdr.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("failed to embed the herdr app icon");
}

#[cfg(not(windows))]
fn embed_windows_icon() {}
