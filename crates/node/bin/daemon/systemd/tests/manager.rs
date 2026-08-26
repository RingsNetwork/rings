//! Exercises systemd lifecycle command behavior.

use super::*;
use crate::daemon::report_started;
#[test]
fn config_home_accepts_only_absolute_xdg_paths() {
    let home = Path::new("/home/test");

    assert_eq!(
        systemd_config_home(home, Some(Path::new("/srv/test-config"))),
        Path::new("/srv/test-config")
    );
    assert_eq!(
        systemd_config_home(home, None),
        Path::new("/home/test/.config")
    );
    assert_eq!(
        systemd_config_home(home, Some(Path::new("relative-config"))),
        Path::new("/home/test/.config")
    );
}

#[test]
fn start_reporting_includes_the_systemd_exit_cause() -> Result<(), DaemonError> {
    let failed_status = "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nUnitFileState=enabled\nExecMainCode=1\nExecMainStatus=78\nResult=exit-code\n";
    let root = test_root("start-reporting-failure");
    let mut steps = vec![
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"], ""),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "enable", SYSTEMD_UNIT], ""),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT], ""),
    ];
    fill_poll_budget(&mut steps, 0, || {
        CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, failed_status)
    });
    let runner = ScriptedCommandRunner::new(steps);
    let manager = test_manager(&root, runner);
    let spec = service_spec(&LogLevel::Info, &RuntimeFlavor::MultiThread)?;
    let status = manager.start(&spec)?;

    let error = report_started(&manager, status);
    let expected = DaemonStatus::installed(
        DaemonState::Restarting(Some(DaemonFailure::described("exit code 78"))),
        AutostartState::Enabled,
    );

    assert!(matches!(
        error,
        Err(DaemonError::ServiceDidNotStart { status }) if status == expected
    ));
    Ok(())
}

#[test]
fn start_waits_through_auto_restart_until_the_unit_runs() -> Result<(), DaemonError> {
    let root = test_root("start-auto-restart-pending");
    let runner = ScriptedCommandRunner::new([
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"], ""),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "enable", SYSTEMD_UNIT], ""),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT], ""),
        CommandStep::success(
            SYSTEMCTL,
            &SYSTEMD_STATUS_ARGS,
            "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nUnitFileState=enabled\nExecMainCode=1\nExecMainStatus=78\nResult=exit-code\n",
        ),
        CommandStep::success(
            SYSTEMCTL,
            &SYSTEMD_STATUS_ARGS,
            "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\nResult=success\n",
        ),
    ]);
    let manager = test_manager(&root, runner);
    let spec = service_spec(&LogLevel::Info, &RuntimeFlavor::MultiThread)?;

    let status = manager.start(&spec)?;

    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Running, AutostartState::Enabled)
    );
    Ok(())
}

#[test]
fn stop_targets_an_active_unit_when_the_local_definition_is_missing() -> Result<(), DaemonError> {
    let runner = ScriptedCommandRunner::new([
        CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, DETACHED_RUNNING_STATUS),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "stop", SYSTEMD_UNIT], ""),
        CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, NOT_INSTALLED_STATUS),
    ]);
    let manager = detached_manager(runner);

    let status = manager.stop()?;

    assert_eq!(status, DaemonStatus::NotInstalled);
    Ok(())
}

#[test]
fn status_preserves_systemctl_connection_failures() {
    let runner = ScriptedCommandRunner::new([CommandStep::failure(
        SYSTEMCTL,
        &SYSTEMD_STATUS_ARGS,
        1,
        "Failed to connect to bus",
    )]);
    let manager = detached_manager(runner);

    let result = manager.observe();

    assert!(matches!(result, Err(DaemonError::CommandFailed(_))));
}

#[test]
fn start_installs_definition_then_reload_enables_and_restarts() -> Result<(), DaemonError> {
    let root = test_root("start-sequence");
    let runner = ScriptedCommandRunner::new([
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"], ""),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "enable", SYSTEMD_UNIT], ""),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT], ""),
        CommandStep::success(
            SYSTEMCTL,
            &SYSTEMD_STATUS_ARGS,
            "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\nExecMainCode=0\nExecMainStatus=0\nResult=success\n",
        ),
    ]);
    let manager = test_manager(&root, runner);
    let spec = service_spec(&LogLevel::Info, &RuntimeFlavor::MultiThread)?;

    let status = manager.start(&spec)?;

    assert!(manager.unit_path.is_file());
    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Running, AutostartState::Enabled)
    );
    Ok(())
}

#[test]
fn restart_targets_an_active_unit_when_the_local_definition_is_missing() -> Result<(), DaemonError>
{
    let runner = ScriptedCommandRunner::new([
        CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, DETACHED_RUNNING_STATUS),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT], ""),
        CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, DETACHED_RUNNING_STATUS),
    ]);
    let manager = detached_manager(runner);

    let status = manager.restart()?;

    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Running, AutostartState::Unknown)
    );
    Ok(())
}

#[test]
fn restart_of_installed_unit_preserves_autostart() -> Result<(), DaemonError> {
    let root = test_root("restart-sequence");
    let runner = ScriptedCommandRunner::new([
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"], ""),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "restart", SYSTEMD_UNIT], ""),
        CommandStep::success(
            SYSTEMCTL,
            &SYSTEMD_STATUS_ARGS,
            "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=disabled\nResult=success\n",
        ),
    ]);
    let manager = test_manager(&root, runner);
    write_atomic(&manager.unit_path, "installed")?;

    let status = manager.restart()?;

    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Running, AutostartState::Disabled)
    );
    Ok(())
}

#[test]
fn stop_returns_the_inactive_manager_verdict_without_issuing_stop() -> Result<(), DaemonError> {
    for (status_output, expected) in [
        (NOT_INSTALLED_STATUS, DaemonStatus::NotInstalled),
        (
            "LoadState=masked\nActiveState=inactive\nSubState=dead\nUnitFileState=masked\n",
            DaemonStatus::installed(DaemonState::Unavailable, AutostartState::Unavailable),
        ),
    ] {
        let runner = ScriptedCommandRunner::new([CommandStep::success(
            SYSTEMCTL,
            &SYSTEMD_STATUS_ARGS,
            status_output,
        )]);
        let manager = detached_manager(runner);

        let status = manager.stop()?;

        assert_eq!(status, expected);
    }
    Ok(())
}

#[test]
fn missing_manager_record_with_local_definition_is_installed_and_stopped() -> Result<(), DaemonError>
{
    let root = test_root("missing-record-local-definition");
    let runner = ScriptedCommandRunner::new([
        CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, NOT_INSTALLED_STATUS),
        CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, NOT_INSTALLED_STATUS),
    ]);
    let manager = test_manager(&root, runner);
    write_atomic(&manager.unit_path, "installed")?;

    assert_eq!(
        manager.observe()?,
        DaemonStatus::installed(DaemonState::Stopped, AutostartState::Unknown)
    );
    assert_eq!(
        manager.stop()?,
        DaemonStatus::installed(DaemonState::Stopped, AutostartState::Unknown)
    );
    Ok(())
}

#[test]
fn restart_rejects_a_not_installed_unit() {
    let runner = ScriptedCommandRunner::new([CommandStep::success(
        SYSTEMCTL,
        &SYSTEMD_STATUS_ARGS,
        NOT_INSTALLED_STATUS,
    )]);
    let manager = detached_manager(runner);

    let result = manager.restart();

    assert!(matches!(
        result,
        Err(DaemonError::ServiceNotInstalled { .. })
    ));
}

#[test]
fn restart_rejects_an_unavailable_unit_with_a_typed_error() {
    let runner = ScriptedCommandRunner::new([CommandStep::success(
        SYSTEMCTL,
        &SYSTEMD_STATUS_ARGS,
        "LoadState=masked\nActiveState=inactive\nSubState=dead\nUnitFileState=masked\n",
    )]);
    let manager = detached_manager(runner);

    let result = manager.restart();

    assert!(matches!(
        result,
        Err(DaemonError::Systemd(SystemdError::UnitUnavailable { .. }))
    ));
}
