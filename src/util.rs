#![warn(missing_docs)]
//! Utils for project.

use std::env;

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

/// load_config env file from path if available
pub fn load_config() {
    let mut v = env::args();
    while let Some(item) = v.next() {
        if item.eq("-c") || item.eq("--config_file") {
            let config = v.next();
            if let Some(c) = config {
                dotenv::from_path(c).ok();
            }
            break;
        }
    }
}
