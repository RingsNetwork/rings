#![warn(missing_docs)]
//! Utils for project.

/// build_version of program
pub fn build_version() -> String {
    let mut infos = vec![];
    if let Some(version) = option_env!("CARGO_PKG_VERSION") {
        infos.push(version);
    };
    if let Some(git_hash) = option_env!("GIT_SHORT_HASH") {
        infos.push(git_hash);
    }
    infos.join("-")
}
