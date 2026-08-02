use std::process::Command;

fn main() {
    // Allow CI to inject the hash directly via environment (e.g. GITHUB_SHA)
    // when git is not available (container builds without .git directory).
    let git_hash = std::env::var("GIT_HASH")
        .ok()
        .map(|s| s[..s.len().min(7)].to_string()) // truncate to short hash
        .filter(|s| !s.is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["describe", "--always", "--dirty=+dev"])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_HASH={git_hash}");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-env-changed=GIT_HASH");
}
