//! Generates interactive landing-page copy from the initial HTML document.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

struct ProjectField {
    marker: &'static str,
    tag: &'static str,
    constant: &'static str,
    documentation: &'static str,
}

const PROJECT_FIELDS: [ProjectField; 3] = [
    ProjectField {
        marker: "name",
        tag: "p",
        constant: "PROJECT_NAME",
        documentation: "Project name read from the initial HTML document.",
    },
    ProjectField {
        marker: "tagline",
        tag: "h1",
        constant: "PROJECT_TAGLINE",
        documentation: "Primary project positioning read from the initial HTML document.",
    },
    ProjectField {
        marker: "introduction",
        tag: "p",
        constant: "PROJECT_INTRODUCTION",
        documentation: "Project introduction read from the initial HTML document.",
    },
];

fn main() -> ExitCode {
    println!("cargo:rerun-if-changed=index.html");
    match generate_project_content() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cannot generate project content: {error}");
            ExitCode::FAILURE
        }
    }
}

fn generate_project_content() -> Result<(), String> {
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "CARGO_MANIFEST_DIR is unavailable".to_owned())?;
    let index_path = manifest_dir.join("index.html");
    let document = fs::read_to_string(&index_path)
        .map_err(|error| format!("cannot read {}: {error}", index_path.display()))?;
    let mut generated = String::from("// Generated from index.html; do not edit.\n\n");

    for field in &PROJECT_FIELDS {
        let value = extract_field(&document, field)?;
        writeln!(
            generated,
            "/// {}\npub(crate) const {}: &str = {value:?};\n",
            field.documentation, field.constant
        )
        .map_err(|error| format!("cannot format generated project content: {error}"))?;
    }

    let output_dir = env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| "OUT_DIR is unavailable".to_owned())?;
    let output_path = output_dir.join("project_content.rs");
    fs::write(&output_path, generated)
        .map_err(|error| format!("cannot write {}: {error}", output_path.display()))
}

fn extract_field(document: &str, field: &ProjectField) -> Result<String, String> {
    let marker = format!(r#"data-project-content="{}""#, field.marker);
    let mut matches = document.match_indices(&marker);
    let (marker_offset, _) = matches
        .next()
        .ok_or_else(|| format!("index.html lacks project-content marker {}", field.marker))?;
    if matches.next().is_some() {
        return Err(format!(
            "index.html contains duplicate project-content marker {}",
            field.marker
        ));
    }

    let prefix = document
        .get(..marker_offset)
        .ok_or_else(|| format!("invalid marker offset for {}", field.marker))?;
    let opening_offset = prefix
        .rfind('<')
        .ok_or_else(|| format!("marker {} is outside an HTML element", field.marker))?;
    let opening = document
        .get(opening_offset..marker_offset)
        .ok_or_else(|| format!("invalid opening element for {}", field.marker))?;
    if !opening.starts_with(&format!("<{} ", field.tag)) {
        return Err(format!(
            "project-content marker {} must be on a {} element",
            field.marker, field.tag
        ));
    }

    let after_marker_offset = marker_offset
        .checked_add(marker.len())
        .ok_or_else(|| format!("marker offset overflow for {}", field.marker))?;
    let after_marker = document
        .get(after_marker_offset..)
        .ok_or_else(|| format!("invalid marker boundary for {}", field.marker))?;
    let opening_end = after_marker
        .find('>')
        .ok_or_else(|| format!("unterminated opening element for {}", field.marker))?;
    let content_offset = after_marker_offset
        .checked_add(opening_end + 1)
        .ok_or_else(|| format!("content offset overflow for {}", field.marker))?;
    let remainder = document
        .get(content_offset..)
        .ok_or_else(|| format!("invalid content boundary for {}", field.marker))?;
    let closing = format!("</{}>", field.tag);
    let closing_offset = remainder
        .find(&closing)
        .ok_or_else(|| format!("missing closing element for {}", field.marker))?;
    let content = remainder
        .get(..closing_offset)
        .ok_or_else(|| format!("invalid content range for {}", field.marker))?
        .trim();
    if content.is_empty() || content.contains('<') || content.contains('&') {
        return Err(format!(
            "project-content marker {} must contain plain, non-empty text",
            field.marker
        ));
    }
    Ok(content.to_owned())
}
