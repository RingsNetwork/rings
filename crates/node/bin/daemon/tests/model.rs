//! Pure shared-model, discovery, formatting, and polling invariants.

use super::*;

#[cfg(unix)]
#[test]
fn production_observation_budget_is_shorter_than_delayed_respawns() {
    let retry_count = u32::try_from(MANAGER_OBSERVATION_SCHEDULE.retries).unwrap_or(u32::MAX);
    let budget = MANAGER_OBSERVATION_SCHEDULE
        .interval
        .saturating_mul(retry_count);

    assert!(budget < super::super::systemd::SYSTEMD_RESTART_DELAY);
    assert!(budget < super::super::launchd::OBSERVED_THROTTLE_FLOOR);
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
        "could not execute primary-manager; recovery also failed: could not execute recovery-manager: recovery source"
    );
    assert_eq!(
        std::error::Error::source(&failure).map(ToString::to_string),
        Some("primary source".to_owned())
    );
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
