//! Proves launchd stop confirms absence while reporting the loaded record it acted upon.

use super::*;

#[test]
fn stop_unloads_the_job_and_preserves_enabled_autostart() -> Result<(), DaemonError> {
    let root = test_root("stop-success");
    let domain = TEST_DOMAIN;
    let target = test_target();
    install_test_definition(&root)?;
    let runner = ScriptedCommandRunner::new([
        loaded_service(&target, "state = running\n"),
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
fn stop_reports_the_loaded_job_it_acted_on_even_without_a_plist() -> Result<(), DaemonError> {
    let root = test_root("stop-loaded-without-plist");
    let target = test_target();
    let runner = ScriptedCommandRunner::new([
        loaded_service(&target, "state = running\n"),
        CommandStep::success(LAUNCHCTL, &["bootout", &target], ""),
        missing_service(&target),
        enabled_autostart(TEST_DOMAIN),
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
fn stop_reports_an_absent_job_with_a_plist_as_stopped() -> Result<(), DaemonError> {
    let root = test_root("stop-absent-with-plist");
    let target = test_target();
    install_test_definition(&root)?;
    let manager = scripted_manager(&root, [
        missing_service(&target),
        enabled_autostart(TEST_DOMAIN),
    ]);

    let status = manager.stop()?;

    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Stopped, AutostartState::Enabled)
    );
    Ok(())
}

#[test]
fn stop_reports_an_absent_job_without_a_plist_as_not_installed() -> Result<(), DaemonError> {
    let root = test_root("stop-absent-without-plist");
    let target = test_target();
    let manager = scripted_manager(&root, [missing_service(&target)]);

    let status = manager.stop()?;

    assert_eq!(status, DaemonStatus::NotInstalled);
    Ok(())
}

#[test]
fn stop_reports_a_job_that_never_unloads() -> Result<(), DaemonError> {
    let root = test_root("stop-timeout");
    let target = test_target();
    install_test_definition(&root)?;
    let mut steps = vec![
        loaded_service(&target, "state = running\n"),
        CommandStep::success(LAUNCHCTL, &["bootout", &target], ""),
    ];
    fill_poll_budget(&mut steps, 0, || {
        loaded_service(&target, "state = running\n")
    });
    let manager = test_manager(&root, ScriptedCommandRunner::new(steps));

    let result = manager.stop();

    assert!(matches!(
        result,
        Err(DaemonError::Launchd(LaunchdError::ServiceDidNotUnload))
    ));
    Ok(())
}
