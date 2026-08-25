//! Pure systemd unit rendering with no process or filesystem effects.

use std::time::Duration;

use thiserror::Error;

use super::super::ServiceSpec;

pub(crate) const SYSTEMD_RESTART_DELAY: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub(crate) enum SystemdDefinitionError {
    #[error("working directory contains a line break and cannot be written safely to a systemd unit: {value:?}")]
    ContainsLineBreak { value: String },
    #[error("working directory has leading or trailing ASCII whitespace that systemd would discard: {value:?}")]
    HasBoundaryWhitespace { value: String },
    #[error("working directory ends in a backslash that would continue the systemd unit line: {value:?}")]
    EndsWithBackslash { value: String },
}

pub(crate) fn render_systemd_unit(spec: &ServiceSpec) -> Result<String, SystemdDefinitionError> {
    let command = spec
        .arguments()
        .into_iter()
        .map(systemd_exec_quote)
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!(
        "[Unit]\n\
Description=Rings Network node\n\
Wants=network-online.target\n\
After=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
WorkingDirectory={}\n\
ExecStart={command}\n\
Restart=on-failure\n\
RestartSec={}\n\
TimeoutStopSec=30\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        systemd_working_directory(&spec.working_directory)?,
        SYSTEMD_RESTART_DELAY.as_secs(),
    ))
}

fn systemd_exec_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '$' => quoted.push_str("$$"),
            '%' => quoted.push_str("%%"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

fn systemd_working_directory(value: &str) -> Result<String, SystemdDefinitionError> {
    // Observed with systemd 257.13: WorkingDirectory preserves interior TAB and a single
    // backslash verbatim. Unlike ExecStart, config_parse_working_directory does not C-unescape
    // the value; only percent specifiers require escaping here. A terminal backslash remains the
    // unit reader's line-continuation marker and is therefore rejected.
    if value
        .chars()
        .any(|character| matches!(character, '\n' | '\r'))
    {
        return Err(SystemdDefinitionError::ContainsLineBreak {
            value: value.to_owned(),
        });
    }
    if has_boundary_ascii_whitespace(value) {
        return Err(SystemdDefinitionError::HasBoundaryWhitespace {
            value: value.to_owned(),
        });
    }
    if value.ends_with('\\') {
        return Err(SystemdDefinitionError::EndsWithBackslash {
            value: value.to_owned(),
        });
    }
    Ok(value.replace('%', "%%"))
}

fn has_boundary_ascii_whitespace(value: &str) -> bool {
    value
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_whitespace())
        || value
            .chars()
            .next_back()
            .is_some_and(|character| character.is_ascii_whitespace())
}

#[cfg(test)]
mod tests {
    use rings_node::logging::LogLevel;

    use super::super::super::super::RuntimeFlavor;
    use super::super::super::tests::service_spec;
    use super::super::super::DaemonError;
    use super::*;

    #[test]
    fn definition_quotes_arguments_and_sets_working_directory() -> Result<(), DaemonError> {
        let spec = service_spec(&LogLevel::Warn, &RuntimeFlavor::CurrentThread)?;
        let unit = render_systemd_unit(&spec)?;

        assert!(unit.contains("ExecStart=\"/Users/test user/bin/rings\""));
        assert!(unit.contains("\"/Users/test user/.rings/config&prod.yaml\""));
        assert!(unit.contains("WorkingDirectory=/Users/test user/work"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("RestartSec=5"));
        assert!(unit.contains("WantedBy=default.target"));
        Ok(())
    }

    #[test]
    fn working_directory_preserves_raw_characters_and_escapes_only_specifiers() {
        assert!(matches!(
            systemd_working_directory("/tmp/a\t$HOME/%n/\\rings/\"node\"/'worker'/\u{7}"),
            Ok(path) if path == "/tmp/a\t$HOME/%%n/\\rings/\"node\"/'worker'/\u{7}"
        ));
        assert!(matches!(
            systemd_working_directory("\u{a0}/tmp/rings\u{a0}"),
            Ok(path) if path == "\u{a0}/tmp/rings\u{a0}"
        ));
        assert!(matches!(
            systemd_working_directory("/tmp/rings\\"),
            Err(SystemdDefinitionError::EndsWithBackslash { .. })
        ));
    }

    #[test]
    fn working_directory_rejects_line_reader_mutations() {
        for path in [
            "/tmp/rings\nnode",
            "/tmp/rings\rnode",
            " /tmp/rings",
            "/tmp/rings ",
            "\t/tmp/rings",
            "/tmp/rings\t",
        ] {
            assert!(systemd_working_directory(path).is_err());
        }
    }

    #[test]
    fn exec_quote_escapes_service_manager_expansion_characters() {
        assert_eq!(
            systemd_exec_quote("/tmp/a $HOME/%n/\"rings\""),
            "\"/tmp/a $$HOME/%%n/\\\"rings\\\"\""
        );
    }
}
