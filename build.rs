fn main() {
    // Set VEX_VERSION from git tag at compile time — but only when building
    // from a vex checkout. A crates.io build (`cargo install vex-search`)
    // unpacks into ~/.cargo/registry/src/… with no `.git`; running
    // `git describe` there would walk up into whatever enclosing repo exists
    // (e.g. a dotfiles repo at $HOME) and stamp its tag/hash as vex's version.
    let in_checkout = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".git")
        .exists();
    let version = in_checkout
        .then(|| {
            std::process::Command::new("git")
                .args(["describe", "--tags", "--always"])
                .output()
                .ok()
        })
        .flatten()
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        })
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());

    println!("cargo:rustc-env=VEX_VERSION={version}");
}
