fn main() {
    embuild::espidf::sysenv::output();

    // Capture build-time epoch seconds so the firmware can seed PCF8563 on
    // first boot (when the coin cell is missing or drained and the VL bit is
    // asserted). The build script reruns whenever source files change, so a
    // rebuild will refresh this value automatically.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=BUILD_EPOCH_SECS={now}");

    // The ESP-IDF app descriptor's `App version`/`Compile time` (visible in
    // the boot log) come from a `git describe` cached in esp-idf-sys's CMake
    // build directory at its first configure and are NOT recomputed on later
    // `cargo build`s - see ../docs/remaining-work.md's stale-metadata item. This
    // firmware-level GIT_REV is always current: printed once at boot instead
    // of trusting that field.
    let git_rev = std::process::Command::new("git")
        .args(["describe", "--always", "--dirty", "--tags"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_REV={git_rev}");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-changed=../.git/refs");
}
