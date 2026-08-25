//! Verifies stop waits for the loaded record to disappear and reuses that terminal observation
//! while preserving the independent login-autostart setting.

use super::*;

#[test]
fn stop_unloads_the_job_and_preserves_enabled_autostart() -> Result<(), DaemonError> {
    let root = test_root("stop-success");
    let domain = TEST_DOMAIN;
    let target = test_target();
    install_test_definition(&root)?;
    let runner = ScriptedCommandRunner::new([
        CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
        CommandStep::success(LAUNCHCTL, &["bootout", &target], ""),
        missing_service(&target),
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
    let target = test_target();
    install_test_definition(&root)?;
    let mut steps = vec![
        CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
        CommandStep::success(LAUNCHCTL, &["bootout", &target], ""),
    ];
    for _ in 0..=TEST_OBSERVATION_SCHEDULE.retries {
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
