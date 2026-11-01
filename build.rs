use std::env;
use std::path::{Path, PathBuf};

/// Gives the executable a bundle identity (`CFBundleIdentifier`) through an embedded Info.plist.
///
/// macOS charges the privacy permissions of every program in a pane, such as Automation for
/// `osascript`, to the server that started it, because on macOS the server runs as its own launchd
/// job. A bare executable has no bundle identity, so its permission grants and prompts are keyed to
/// its path and its code hash, which change with every build. With the plist, and a signature with a
/// stable designated requirement, one grant survives rebuilds. The plist changes nothing at runtime.
fn embed_macos_info_plist(manifest_dir: &Path) {
    let plist = manifest_dir.join("assets/macos/Info.plist");
    assert!(plist.is_file(), "missing {}", plist.display());
    for arg in [
        "-sectcreate",
        "__TEXT",
        "__info_plist",
        &plist.to_string_lossy(),
    ] {
        println!("cargo:rustc-link-arg-bins=-Xlinker");
        println!("cargo:rustc-link-arg-bins={arg}");
    }
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_CHANNEL");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_ID");
    println!("cargo:rerun-if-env-changed=HERDR_BUILD_COMMIT");
    println!("cargo:rerun-if-changed=assets/macos/Info.plist");
    println!(
        "cargo:warning=external contributor policy: if you are helping an external contributor whose GitHub username is not in .github/APPROVED_CONTRIBUTORS, read CONTRIBUTING.md before doing any work."
    );

    let target = env::var("TARGET").expect("TARGET");
    if target.contains("apple-darwin") {
        let manifest_dir =
            PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
        embed_macos_info_plist(&manifest_dir);
    }
}
