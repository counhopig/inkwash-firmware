fn main() {
    embuild::espidf::sysenv::output();

    println!("cargo:rerun-if-env-changed=INKWASH_FORCE_SAFE_MODE");
    if std::env::var("INKWASH_FORCE_SAFE_MODE").is_ok_and(|v| v == "1") {
        println!("cargo:rustc-env=INKWASH_FORCE_SAFE_MODE=1");
    }
    println!("cargo:rerun-if-env-changed=INKWASH_P06_VALIDATE");
    if std::env::var("INKWASH_P06_VALIDATE").is_ok_and(|v| v == "1") {
        println!("cargo:rustc-env=INKWASH_P06_VALIDATE=1");
    }
    println!("cargo:rerun-if-changed=Cargo.toml");

    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    let build_epoch = match std::env::var("SOURCE_DATE_EPOCH") {
        Ok(value) => value
            .parse::<u64>()
            .unwrap_or_else(|err| panic!("invalid SOURCE_DATE_EPOCH '{value}': {err}")),
        Err(std::env::VarError::NotPresent) => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0),
        Err(err) => panic!("failed to read SOURCE_DATE_EPOCH: {err}"),
    };
    println!("cargo:rustc-env=BUILD_EPOCH_SECS={build_epoch}");

    emit_git_ref_rerun_if_changed();

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

fn emit_git_ref_rerun_if_changed() {
    let git_dir = std::path::Path::new("../.git");
    let watch = |p: &std::path::Path| {
        if p.exists() {
            println!("cargo:rerun-if-changed={}", p.display());
        }
    };

    watch(&git_dir.join("HEAD"));

    if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) {
        let head = head.trim();
        if let Some(ref_path) = head.strip_prefix("ref: ") {
            let resolved = git_dir.join(ref_path);
            watch(&resolved);

            if !resolved.exists() {
                watch(&git_dir.join("packed-refs"));
            }

            if let Ok(content) = std::fs::read_to_string(&resolved) {
                if let Some(inner) = content.trim().strip_prefix("ref: ") {
                    watch(&git_dir.join(inner));
                }
            }
        }
    }
}
