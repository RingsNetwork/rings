//! Pure launchd definition rendering and manager-exit classification.

use thiserror::Error;

use super::super::ServiceSpec;

pub(super) const LAUNCHD_LABEL: &str = "io.ringsnetwork.node";
// Observed on macOS 15.6.1 (24G90): `launchctl error 113` is service-not-found.
pub(super) const LAUNCHD_SERVICE_NOT_FOUND: i32 = 113;
// Observed on macOS 15.6.1 (24G90): bootstrap of a disabled unloaded label returns 5. Because
// launchctl also uses 5 for unrelated bootstrap failures, the adapter must corroborate Disabled
// through `print-disabled` before mutating the label.
pub(super) const LAUNCHD_BOOTSTRAP_DISABLED: i32 = 5;

#[derive(Debug, Error)]
pub(crate) enum LaunchdDefinitionError {
    // Debug formatting keeps the rejected control character escaped in diagnostics.
    #[error("value contains a character forbidden by XML 1.0 launchd plists: {value:?}")]
    XmlIncompatibleValue { value: String },
}

pub(super) fn is_service_not_found(code: Option<i32>) -> bool {
    code == Some(LAUNCHD_SERVICE_NOT_FOUND)
}

pub(super) fn may_be_disabled_bootstrap(code: Option<i32>) -> bool {
    code == Some(LAUNCHD_BOOTSTRAP_DISABLED)
}

pub(crate) fn render_launchd_plist(
    spec: &ServiceSpec,
    stdout_log: &str,
    stderr_log: &str,
) -> Result<String, LaunchdDefinitionError> {
    let label = xml_string(LAUNCHD_LABEL)?;
    let arguments = spec
        .arguments()
        .into_iter()
        .map(|argument| {
            xml_string(argument).map(|argument| format!("    <string>{argument}</string>\n"))
        })
        .collect::<Result<String, LaunchdDefinitionError>>()?;
    let working_directory = xml_string(&spec.working_directory)?;
    let stdout_log = xml_string(stdout_log)?;
    let stderr_log = xml_string(stderr_log)?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{arguments}  </array>
  <key>WorkingDirectory</key>
  <string>{}</string>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key>
    <false/>
  </dict>
  <key>ProcessType</key>
  <string>Background</string>
  <key>StandardOutPath</key>
  <string>{}</string>
  <key>StandardErrorPath</key>
  <string>{}</string>
</dict>
</plist>
"#,
        working_directory, stdout_log, stderr_log,
    ))
}

fn xml_string(value: &str) -> Result<String, LaunchdDefinitionError> {
    if !value.chars().all(is_xml_1_0_character) {
        Err(LaunchdDefinitionError::XmlIncompatibleValue {
            value: value.to_owned(),
        })
    } else {
        Ok(value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;"))
    }
}

fn is_xml_1_0_character(character: char) -> bool {
    matches!(
        character,
        '\u{9}'
            | '\u{a}'
            | '\u{d}'
            | '\u{20}'..='\u{d7ff}'
            | '\u{e000}'..='\u{fffd}'
            | '\u{10000}'..='\u{10ffff}'
    )
}

#[cfg(test)]
mod tests {
    use rings_node::logging::LogLevel;

    use super::super::super::super::RuntimeFlavor;
    use super::super::super::tests::service_spec;
    use super::super::super::DaemonError;
    use super::*;

    #[test]
    fn definition_preserves_arguments_working_directory_and_xml() -> Result<(), DaemonError> {
        let spec = service_spec(&LogLevel::Warn, &RuntimeFlavor::CurrentThread)?;
        let plist = render_launchd_plist(
            &spec,
            "/Users/test user/.rings/logs/daemon.log",
            "/Users/test user/.rings/logs/daemon.error.log",
        )?;

        assert!(plist.contains("<string>/Users/test user/bin/rings</string>"));
        assert!(plist.contains("<string>/Users/test user/.rings/config&amp;prod.yaml</string>"));
        assert!(plist.contains("<key>WorkingDirectory</key>"));
        assert!(plist.contains("<string>/Users/test user/work</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        Ok(())
    }

    #[test]
    fn definition_rejects_values_forbidden_by_xml_1_0() -> Result<(), DaemonError> {
        for character in [
            '\u{0}', '\u{1}', '\u{8}', '\u{b}', '\u{c}', '\u{e}', '\u{1f}', '\u{fffe}', '\u{ffff}',
        ] {
            let mut spec = service_spec(&LogLevel::Warn, &RuntimeFlavor::CurrentThread)?;
            spec.working_directory = format!("/tmp/rings{character}daemon");
            assert!(matches!(
                render_launchd_plist(
                    &spec,
                    "/tmp/rings-daemon.log",
                    "/tmp/rings-daemon.error.log",
                ),
                Err(LaunchdDefinitionError::XmlIncompatibleValue { .. })
            ));
        }
        Ok(())
    }

    #[test]
    fn definition_accepts_xml_1_0_path_boundaries() -> Result<(), DaemonError> {
        for character in [
            '\u{9}',
            '\u{a}',
            '\u{d}',
            '\u{20}',
            '\u{d7ff}',
            '\u{e000}',
            '\u{fffd}',
            '\u{10000}',
            '\u{10ffff}',
        ] {
            let mut spec = service_spec(&LogLevel::Warn, &RuntimeFlavor::CurrentThread)?;
            spec.working_directory = format!("/tmp/rings{character}daemon");
            assert!(render_launchd_plist(
                &spec,
                "/tmp/rings-daemon.log",
                "/tmp/rings-daemon.error.log",
            )
            .is_ok());
        }
        Ok(())
    }
}
