use super::*;

#[test]
fn stop_unloads_the_job_and_preserves_enabled_autostart() -> Result<(), DaemonError> {
    let root = test_root("stop-success");
    let domain = TEST_DOMAIN;
    let target = TEST_TARGET.to_owned();
    install_test_definition(&root)?;
    let runner = ScriptedCommandRunner::new([
        CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
        CommandStep::success(LAUNCHCTL, &["bootout", &target], ""),
        CommandStep::failure(
            LAUNCHCTL,
            &["print", &target],
            LAUNCHD_SERVICE_NOT_FOUND,
            "Could not find specified service",
        ),
        CommandStep::failure(
            LAUNCHCTL,
            &["print", &target],
            LAUNCHD_SERVICE_NOT_FOUND,
            "Could not find specified service",
        ),
        enabled_autostart(domain),
    ]);
    let manager = test_manager(&root, runner);

    let status = manager.stop()?;

    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Stopped, AutostartState::Enabled)
    );
    Ok(())
}

#[test]
fn stop_reports_a_job_that_never_unloads() -> Result<(), DaemonError> {
    let root = test_root("stop-timeout");
    let target = TEST_TARGET.to_owned();
    install_test_definition(&root)?;
    let mut steps = vec![
        CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
        CommandStep::success(LAUNCHCTL, &["bootout", &target], ""),
    ];
    for _ in 0..=super::super::super::OBSERVATION_RETRIES {
        steps.push(CommandStep::success(
            LAUNCHCTL,
            &["print", &target],
            "state = running\n",
        ));
    }
    let manager = test_manager(&root, ScriptedCommandRunner::new(steps));

    let result = manager.stop();

    assert!(matches!(
        result,
        Err(DaemonError::Launchd(LaunchdError::ServiceDidNotUnload))
    ));
    Ok(())
}

#[test]
fn loaded_job_without_a_plist_still_queries_manager_autostart() -> Result<(), DaemonError> {
    let root = test_root("status-loaded-without-plist");
    let domain = TEST_DOMAIN;
    let target = TEST_TARGET.to_owned();
    let runner = ScriptedCommandRunner::new([
        CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
        enabled_autostart(domain),
    ]);
    let manager = test_manager(&root, runner);

    let status = manager.observe()?;

    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Running, AutostartState::Enabled)
    );
    Ok(())
}
