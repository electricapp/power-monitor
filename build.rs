use std::process::Command;

fn main() {
    println!("cargo:rustc-link-lib=framework=IOKit");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");
    println!("cargo:rustc-link-lib=dylib=IOReport");

    // Build metadata for the phonon-style `version X (built <ts>, <git>)` line.
    // Re-run when HEAD or the index moves so the hash/dirty flag stay fresh.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let git = git_describe().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=PM_GIT={git}");

    let ts = build_timestamp();
    println!("cargo:rustc-env=PM_BUILD_TIME={ts}");
}

/// Short commit hash, with `-dirty` appended when the work-tree has changes.
fn git_describe() -> Option<String> {
    let hash = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !hash.status.success() {
        return None;
    }
    let mut s = String::from_utf8(hash.stdout).ok()?.trim().to_string();
    if s.is_empty() {
        return None;
    }
    if let Ok(st) = Command::new("git").args(["status", "--porcelain"]).output()
        && st.status.success()
        && !st.stdout.is_empty()
    {
        s.push_str("-dirty");
    }
    Some(s)
}

/// UTC build time as RFC 3339. Honors SOURCE_DATE_EPOCH for reproducible builds.
fn build_timestamp() -> String {
    let fmt = "+%Y-%m-%dT%H:%M:%SZ";
    let out = match std::env::var("SOURCE_DATE_EPOCH") {
        // BSD/macOS `date -r <epoch>`; this crate is macOS-only.
        Ok(epoch) => Command::new("date")
            .args(["-u", "-r", &epoch, fmt])
            .output(),
        Err(_) => Command::new("date").args(["-u", fmt]).output(),
    };
    if let Ok(out) = out
        && out.status.success()
        && let Ok(s) = String::from_utf8(out.stdout)
    {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    "unknown".to_string()
}
