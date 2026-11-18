use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn main() {
    println!("cargo:rerun-if-env-changed=KABELSALAT_COMMIT");
    // Package builds without a .git (tarballs, distro builds) pass the commit in.
    let commit = std::env::var("KABELSALAT_COMMIT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| git(&["rev-parse", "--short=7", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=KABELSALAT_COMMIT={commit}");

    for path in ["HEAD", "packed-refs"] {
        if let Some(p) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(p) = git(&["rev-parse", "--git-path", &r]) {
            println!("cargo:rerun-if-changed={p}");
        }
    }
}
