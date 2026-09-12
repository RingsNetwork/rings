//! Repository boundary test for checked-in logo artifacts.

use std::fs;
use std::io;
use std::path::PathBuf;

use rings_logo::generate_assets;

#[test]
fn checked_in_assets_match_the_generator() -> Result<(), Box<dyn std::error::Error>> {
    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    for asset in generate_assets()? {
        let path = repository_root.join(asset.path());
        let actual = fs::read_to_string(&path)?;
        if actual != asset.contents() {
            return Err(io::Error::other(format!(
                "{} differs; run `cargo run -p rings-logo -- generate`",
                path.display()
            ))
            .into());
        }
    }
    Ok(())
}
