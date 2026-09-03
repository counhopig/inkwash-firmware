fn main() {
    embuild::espidf::sysenv::output();

    // Test-only safe-mode injection: when the build environment sets
    // INKWASH_FORCE_SAFE_MODE=1, the compiled binary forces the minimum
    // safe-mode entry on a healthy boot (see main.rs run_safe_mode) so the
    // safe path is device-verifiable without destroying NVS/RTC. The normal
    // build (unset) is unaffected. Implemented as a build-time env ->
    // rustc-env so only the final crate rebuilds (a --cfg via RUSTFLAGS
    // would invalidate every dependency).
    // Cargo reruns build.rs when this env toggles, so a normal build (env
    // unset) reliably drops the flag after a force-safe-mode test build.
    println!("cargo:rerun-if-env-changed=INKWASH_FORCE_SAFE_MODE");
    if std::env::var("INKWASH_FORCE_SAFE_MODE").is_ok_and(|v| v == "1") {
        println!("cargo:rustc-env=INKWASH_FORCE_SAFE_MODE=1");
    }

    // Capture build-time epoch seconds so the firmware can seed PCF8563 on
    // first boot (when the coin cell is missing or drained and the VL bit is
    // asserted). The build script reruns whenever source files change, so a
    // rebuild will refresh this value automatically.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=BUILD_EPOCH_SECS={now}");

    // Emit rerun-if-changed for the git ref this build's GIT_REV derives
    // from, so committing on the current branch (which rewrites the branch
    // file under .git/refs/heads/...) reliably triggers a rebuild of the
    // crate and a fresh embedded revision. `cargo:rerun-if-changed` on the
    // whole .git/refs directory is NOT reliable: Cargo does not recursively
    // watch directory contents, so the build script does not rerun when the
    // checked-out branch advances and only the branch file changes.
    //
    // Resolution order:
    //   1. .git/HEAD is usually the symbolic ref `ref: refs/heads/<branch>`.
    //      Emit rerun-if-changed on .git/<ref-path> (the branch file) plus
    //      .git/HEAD itself.
    //   2. Detached HEAD: HEAD holds a raw object id; emitting rerun on
    //      .git/HEAD alone is correct (any checkout rewrites it).
    //   3. packed-refs: when the resolved ref is not a loose file (e.g. it
    //      lives in .git/packed-refs), watch .git/packed-refs too.
    emit_git_ref_rerun_if_changed();

    // The ESP-IDF app descriptor's `App version`/`Compile time` (visible in
    // the boot log) come from a `git describe` cached in esp-idf-sys's CMake
    // build directory at its first configure and are NOT recomputed on later
    // `cargo build`s, so this firmware-level GIT_REV is always current:
    // printed once at boot instead of trusting that field.
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
}

/// Emit `cargo:rerun-if-changed` lines for the git ref(s) that determine
/// `git describe` output. Paths are relative to the crate directory (where
/// build.rs runs), hence the leading `../`.
fn emit_git_ref_rerun_if_changed() {
    let git_dir = std::path::Path::new("../.git");
    let watch = |p: &std::path::Path| {
        // Cargo only treats existing paths as watchable; missing paths are
        // ignored silently, so always also watch the directory-less HEAD.
        if p.exists() {
            println!("cargo:rerun-if-changed={}", p.display());
        }
    };

    // HEAD is always watched: detached checkouts and branch switches rewrite
    // it, and `git describe --dirty` output depends on which commit HEAD
    // points at even when the ref file is unchanged (e.g. `git reset`).
    watch(&git_dir.join("HEAD"));

    // Resolve the symbolic ref, if HEAD is one.
    if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) {
        let head = head.trim();
        if let Some(ref_path) = head.strip_prefix("ref: ") {
            let resolved = git_dir.join(ref_path);
            watch(&resolved);
            // A ref whose loose file is absent lives in packed-refs; watch it
            // so `git pack-refs` / ref updates that only rewrite the pack
            // still rerun the build.
            if !resolved.exists() {
                watch(&git_dir.join("packed-refs"));
            }
            // refs/remotes/*/HEAD can be a symref to another ref; handle one
            // level of indirection for robustness (rare on the device repo).
            if let Ok(content) = std::fs::read_to_string(&resolved) {
                if let Some(inner) = content.trim().strip_prefix("ref: ") {
                    watch(&git_dir.join(inner));
                }
            }
        } else {
            // Detached HEAD: HEAD itself is the whole story (already watched).
        }
    }
}
