use super::*;

#[test]
fn service_spec_resolves_environment_paths_once_before_rendering() -> io::Result<()> {
    let root = TestRoot::new("shared", "service-spec-discovery");
    fs::create_dir_all(&*root)?;
    let config = root.join("config.yaml");
    fs::write(&config, "config")?;
    let executable = root.join("bin/rings");
    let options = WorkerOptions {
        log_level: LogLevel::Warn,
        runtime: RuntimeFlavor::CurrentThread,
    };

    let spec = ServiceSpec::from_paths("config.yaml", options, &executable, &root)
        .map_err(io::Error::other)?;

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
    let mut observe = || {
        observations
            .next()
            .ok_or_else(|| io::Error::other("polled after settling"))
    };

    let result = poll_until(&mut observe, |value| *value == 2)?;

    assert_eq!(result, 2);
    assert_eq!(observations.next(), Some(3));
    Ok(())
}

#[test]
fn polling_returns_the_final_observation_after_every_retry() -> Result<(), io::Error> {
    let mut calls = 0;
    let mut observe = || {
        calls += 1;
        Ok::<usize, io::Error>(calls)
    };

    let result = poll_until(&mut observe, |_| false)?;

    assert_eq!(result, OBSERVATION_RETRIES + 1);
    assert_eq!(calls, OBSERVATION_RETRIES + 1);
    Ok(())
}
