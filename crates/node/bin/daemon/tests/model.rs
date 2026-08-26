//! Exercises the shared daemon command model.

use super::*;

#[cfg(unix)]
#[test]
fn production_observation_budget_is_shorter_than_our_systemd_restart_delay() {
    let retry_count = u32::try_from(MANAGER_OBSERVATION_SCHEDULE.retries).unwrap_or(u32::MAX);
    let budget = MANAGER_OBSERVATION_SCHEDULE
        .interval
        .saturating_mul(retry_count);

    assert!(budget < super::super::systemd::SYSTEMD_RESTART_DELAY);
}

#[test]
fn service_spec_discovery_captures_paths_consumed_by_renderers() -> io::Result<()> {
    let root = TestRoot::new("shared", "service-spec-discovery");
    fs::create_dir_all(&*root)?;
    let config = root.join("config.yaml");
    fs::write(&config, "config")?;
    let executable = root.join("bin/rings");
    let options = WorkerOptions {
        log_level: LogLevel::Warn,
        runtime: RuntimeFlavor::CurrentThread,
    };

    let spec = ServiceSpec::discover_with(
        "config.yaml",
        options,
        || Ok(executable.clone()),
        || Ok(root.to_path_buf()),
    )
    .map_err(io::Error::other)?;

    #[cfg(unix)]
    super::super::launchd::model::render_launchd_plist(&spec, "/tmp/out", "/tmp/error")
        .map_err(io::Error::other)?;
    #[cfg(unix)]
    super::super::systemd::model::render_systemd_unit(&spec).map_err(io::Error::other)?;

    assert_eq!(spec.executable, executable.to_string_lossy());
    assert_eq!(spec.config, config.canonicalize()?.to_string_lossy());
    assert_eq!(spec.working_directory, root.to_string_lossy());
    assert_eq!(spec.log_level, "warn");
    assert_eq!(spec.runtime, "current-thread");
    Ok(())
}

#[test]
fn recovery_failure_preserves_primary_and_cleanup_errors() {
    let failure = primary_with_recovery(
        io::Error::other("primary sentinel"),
        Err(io::Error::other("cleanup sentinel")),
    );

    assert!(matches!(
        failure,
        RecoveryFailure::Both { primary, recovery }
            if primary.to_string() == "primary sentinel"
                && recovery.to_string() == "cleanup sentinel"
    ));
}

#[test]
fn recovery_failure_display_keeps_the_recovery_source_chain() {
    let failure = RecoveryFailure::Both {
        primary: Box::new(DaemonError::ExecuteCommand {
            program: "primary-manager",
            source: io::Error::other("primary source"),
        }),
        recovery: Box::new(DaemonError::ExecuteCommand {
            program: "recovery-manager",
            source: io::Error::other("recovery source"),
        }),
    };

    assert_eq!(
        failure.to_string(),
        "could not execute primary-manager: primary source; recovery also failed: could not execute recovery-manager: recovery source"
    );
    assert!(std::error::Error::source(&failure).is_none());
}

#[test]
fn primary_recovery_failure_renders_its_complete_chain_without_a_skipped_link() {
    let failure = RecoveryFailure::Primary(Box::new(DaemonError::ExecuteCommand {
        program: "primary-manager",
        source: io::Error::other("primary source"),
    }));

    assert_eq!(
        failure.to_string(),
        "could not execute primary-manager: primary source"
    );
    assert!(std::error::Error::source(&failure).is_none());
}

#[test]
fn top_level_io_error_does_not_repeat_its_source() {
    let error = DaemonError::CurrentDirectory {
        source: io::Error::other("source sentinel"),
    };

    assert_eq!(
        error.to_string(),
        "could not resolve the current working directory"
    );
    assert_eq!(
        std::error::Error::source(&error).map(ToString::to_string),
        Some("source sentinel".to_owned())
    );
}

#[test]
fn polling_stops_at_the_first_settled_observation() -> Result<(), io::Error> {
    let mut observations = [0, 1, 2, 3].into_iter();
    let observe = || {
        observations
            .next()
            .ok_or_else(|| io::Error::other("polled after settling"))
    };

    let result = poll_until(TEST_OBSERVATION_SCHEDULE, observe, |value| *value == 2)?;

    assert_eq!(result, 2);
    assert_eq!(observations.next(), Some(3));
    Ok(())
}

#[test]
fn polling_returns_the_final_observation_after_every_retry() -> Result<(), io::Error> {
    let mut calls = 0;
    let observe = || {
        calls += 1;
        Ok::<usize, io::Error>(calls)
    };

    let result = poll_until(TEST_OBSERVATION_SCHEDULE, observe, |_| false)?;

    assert_eq!(result, TEST_OBSERVATION_SCHEDULE.retries + 1);
    assert_eq!(calls, TEST_OBSERVATION_SCHEDULE.retries + 1);
    Ok(())
}

#[test]
fn generated_worker_arguments_parse_for_every_cli_value() -> Result<(), DaemonError> {
    for log_level in LogLevel::value_variants() {
        for runtime in RuntimeFlavor::value_variants() {
            let spec = service_spec(log_level, runtime)?;
            let expected = Some((spec.log_level.clone(), spec.runtime.clone()));
            let parsed_names = Cli::try_parse_from(spec.arguments())
                .ok()
                .and_then(|parsed| {
                    Some((
                        parsed.log_level.to_possible_value()?.get_name().to_owned(),
                        parsed.runtime.to_possible_value()?.get_name().to_owned(),
                    ))
                });
            assert_eq!(parsed_names, expected);
        }
    }
    Ok(())
}

#[test]
fn human_command_format_does_not_apply_service_manager_escaping() {
    let command = format_command("rings", &["run", "$HOME/%n"]);

    assert_eq!(command, "\"rings\" \"run\" \"$HOME/%n\"");
}

#[test]
fn atomic_write_removes_temporary_file_when_install_fails() -> io::Result<()> {
    let root = TestRoot::new("shared", "atomic-install-failure");
    let target = root.join("definition");
    let temporary = root.join(format!(".definition.{}.tmp", std::process::id()));
    fs::create_dir_all(&target)?;

    let result = write_atomic(&target, "definition");

    assert!(matches!(
        result,
        Err(DaemonError::InstallServiceDefinition { .. })
    ));
    assert!(!temporary.exists());
    Ok(())
}

#[test]
fn definition_failure_location_keeps_target_and_leftover_roles_distinct() {
    let target = Path::new("definition.plist");
    let temporary = Path::new(".definition.plist.42.tmp");
    let cleanup_succeeded = Ok(());
    let cleanup_failed = Err(io::Error::other("cleanup failed"));

    assert_eq!(
        definition_failure_location(target, temporary, &cleanup_succeeded),
        DefinitionFailureLocation {
            target: target.to_path_buf(),
            leftover_temporary: None,
        }
    );
    assert_eq!(
        definition_failure_location(target, temporary, &cleanup_failed),
        DefinitionFailureLocation {
            target: target.to_path_buf(),
            leftover_temporary: Some(temporary.to_path_buf()),
        }
    );
}

#[test]
fn definition_failure_message_names_target_and_leftover_temporary_artifact() {
    let error = DaemonError::InstallServiceDefinition {
        location: DefinitionFailureLocation {
            target: PathBuf::from("definition.plist"),
            leftover_temporary: Some(PathBuf::from(".definition.plist.42.tmp")),
        },
        failure: RecoveryFailure::Primary(io::Error::other("rename failed")),
    };

    assert_eq!(
        error.to_string(),
        "could not install service definition at definition.plist; temporary artifact remains at .definition.plist.42.tmp"
    );
}

#[test]
fn atomic_write_removes_partial_file_when_write_fails() {
    let root = TestRoot::new("shared", "atomic-write-failure");
    let target = root.join("definition");
    let temporary = root.join(format!(".definition.{}.tmp", std::process::id()));

    let result = write_atomic_with(&target, "definition", |path, contents| {
        fs::write(path, contents)?;
        Err(io::Error::other("injected write failure"))
    });

    assert!(matches!(
        result,
        Err(DaemonError::WriteServiceDefinition { location, .. })
            if location.target == target && location.leftover_temporary.is_none()
    ));
    assert!(!temporary.exists());
}

#[test]
fn atomic_write_distinguishes_a_missing_file_name_from_non_utf8() {
    let result = write_atomic(Path::new("/"), "definition");

    assert!(matches!(
        result,
        Err(DaemonError::ServiceDefinitionPathHasNoFileName { path })
            if path == Path::new("/")
    ));
}

#[cfg(unix)]
#[test]
fn atomic_write_validates_non_utf8_name_before_creating_parent() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let root = TestRoot::new("shared", "atomic-non-utf8");
    let parent = root.join("new-parent");
    let target = parent.join(OsStr::from_bytes(b"definition-\xff"));

    let result = write_atomic(&target, "definition");

    assert!(matches!(result, Err(DaemonError::NonUtf8Path { .. })));
    assert!(!parent.exists());
}

#[test]
fn config_path_reports_missing_file_with_init_guidance() {
    let root = TestRoot::new("shared", "missing-config");
    let missing = root.join("config.yaml");

    let error = resolve_config_path(missing.to_string_lossy().as_ref(), &root);

    assert!(matches!(
        &error,
        Err(DaemonError::ConfigNotFound { path }) if *path == missing
    ));
    assert!(error
        .as_ref()
        .err()
        .is_some_and(|error| error.to_string().contains("run `rings init` first")));
}

#[test]
fn config_path_canonicalizes_an_existing_file() -> io::Result<()> {
    let root = TestRoot::new("shared", "existing-config");
    fs::create_dir_all(&*root)?;
    let config = root.join("config.yaml");
    fs::write(&config, "config")?;

    let resolved = resolve_config_path(config.to_string_lossy().as_ref(), &root);

    assert_eq!(resolved.ok(), config.canonicalize().ok());
    Ok(())
}

#[test]
fn config_path_resolves_relative_to_the_captured_working_directory() -> io::Result<()> {
    let root = TestRoot::new("shared", "relative-config");
    fs::create_dir_all(&*root)?;
    let config = root.join("relative.yaml");
    fs::write(&config, "config")?;

    let resolved = resolve_config_path("relative.yaml", &root);

    assert_eq!(resolved.ok(), config.canonicalize().ok());
    Ok(())
}

#[test]
fn config_path_expands_home_before_using_the_working_directory() {
    let root = TestRoot::new("shared", "home-config");
    let missing = "~/.rings/codex-daemon-review-missing.yaml";

    let error = resolve_config_path(missing, &root);

    assert!(matches!(
        error,
        Err(DaemonError::ConfigNotFound { path }) if path.is_absolute() && !path.starts_with(&*root)
    ));
}
