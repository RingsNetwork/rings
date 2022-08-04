use std::process::Command;
fn main() {
    // note: add error checking yourself.
    if let Ok(output) = Command::new("git")
        .args(&["rev-parse", "--short", "HEAD"])
        .output()
    {
        let git_short_hash = String::from_utf8(output.stdout).unwrap();
        println!("cargo:rustc-env=GIT_SHORT_HASH={}", git_short_hash);
    }
}
