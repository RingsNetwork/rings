//! Proves systemd commands preserve manager evidence and typed loadability failures.

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
    let failed_status = "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nUnitFileState=disabled\nExecMainCode=1\nExecMainStatus=78\nResult=exit-code\n";
    let root = test_root("start-reporting-failure");
    let mut steps = vec![
        CommandStep::success(
            SYSTEMCTL,
            &SYSTEMD_STATUS_ARGS,
            "LoadState=loaded\nActiveState=inactive\nSubState=dead\nUnitFileState=disabled\nResult=success\n",
        ),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"], ""),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "start", SYSTEMD_UNIT], ""),
    ];
    fill_poll_budget(&mut steps, 0, || {
        CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, failed_status)
    });
    let runner = ScriptedCommandRunner::new(steps);
    let manager = test_manager(&root, runner);
    write_atomic(&manager.unit_path, "installed")?;
    let status = manager.start()?;

    let error = report_started(&manager, status);
    let expected = DaemonStatus::installed(
        DaemonState::Restarting(Some(DaemonFailure::described("exit code 78"))),
        AutostartState::Disabled,
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
        CommandStep::success(
            SYSTEMCTL,
            &SYSTEMD_STATUS_ARGS,
            "LoadState=loaded\nActiveState=inactive\nSubState=dead\nUnitFileState=disabled\nResult=success\n",
        ),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"], ""),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "start", SYSTEMD_UNIT], ""),
        CommandStep::success(
            SYSTEMCTL,
            &SYSTEMD_STATUS_ARGS,
            "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nUnitFileState=disabled\nExecMainCode=1\nExecMainStatus=78\nResult=exit-code\n",
        ),
        CommandStep::success(
            SYSTEMCTL,
            &SYSTEMD_STATUS_ARGS,
            "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=disabled\nResult=success\n",
        ),
    ]);
    let manager = test_manager(&root, runner);
    write_atomic(&manager.unit_path, "installed")?;

    let status = manager.start()?;

    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Running, AutostartState::Disabled)
    );
    Ok(())
}

#[test]
fn start_is_idempotent_for_an_active_masked_unit() -> Result<(), DaemonError> {
    let runner = ScriptedCommandRunner::new([CommandStep::success(
        SYSTEMCTL,
        &SYSTEMD_STATUS_ARGS,
        "LoadState=masked\nActiveState=active\nSubState=running\nUnitFileState=masked\nResult=success\n",
    )]);
    let manager = detached_manager(runner);

    let status = manager.start()?;

    assert_eq!(
        status,
        DaemonStatus::installed(
            DaemonState::Running,
            AutostartState::Reported("unavailable"),
        )
    );
    Ok(())
}

#[test]
fn stop_targets_an_active_unit_when_the_local_definition_is_missing() -> Result<(), DaemonError> {
    let runner = ScriptedCommandRunner::new([
        CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, DETACHED_RUNNING_STATUS),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "stop", SYSTEMD_UNIT], ""),
    ]);
    let manager = detached_manager(runner);

    let status = manager.stop()?;

    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Stopped, AutostartState::Reported("unknown"),)
    );
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
fn install_writes_definition_and_enables_without_starting() -> Result<(), DaemonError> {
    let root = test_root("install-sequence");
    let runner = ScriptedCommandRunner::new([
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"], ""),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "enable", SYSTEMD_UNIT], ""),
        CommandStep::success(
            SYSTEMCTL,
            &SYSTEMD_STATUS_ARGS,
            "LoadState=loaded\nActiveState=inactive\nSubState=dead\nUnitFileState=enabled\nResult=success\n",
        ),
    ]);
    let manager = test_manager(&root, runner);
    let spec = service_spec(&LogLevel::Info, &RuntimeFlavor::MultiThread)?;

    let status = manager.install(&spec)?;

    assert!(manager.unit_path.is_file());
    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Stopped, AutostartState::Enabled)
    );
    Ok(())
}

#[test]
fn uninstall_stops_disables_and_removes_the_definition() -> Result<(), DaemonError> {
    let root = test_root("uninstall-sequence");
    let runner = ScriptedCommandRunner::new([
        CommandStep::success(
            SYSTEMCTL,
            &SYSTEMD_STATUS_ARGS,
            "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\nResult=success\n",
        ),
        CommandStep::success(
            SYSTEMCTL,
            &[SYSTEMD_USER_ARG, "disable", "--now", SYSTEMD_UNIT],
            "",
        ),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"], ""),
    ]);
    let manager = test_manager(&root, runner);
    write_atomic(&manager.unit_path, "installed")?;

    let status = manager.uninstall()?;

    assert_eq!(status, DaemonStatus::NotInstalled);
    assert!(!manager.unit_path.exists());
    Ok(())
}

#[test]
fn uninstall_is_idempotent_when_no_definition_or_process_exists() -> Result<(), DaemonError> {
    let runner = ScriptedCommandRunner::new([CommandStep::success(
        SYSTEMCTL,
        &SYSTEMD_STATUS_ARGS,
        NOT_INSTALLED_STATUS,
    )]);
    let manager = detached_manager(runner);

    assert_eq!(manager.uninstall()?, DaemonStatus::NotInstalled);
    Ok(())
}

#[test]
fn uninstall_disables_a_dangling_enabled_registration() -> Result<(), DaemonError> {
    let runner = ScriptedCommandRunner::new([
        CommandStep::success(
            SYSTEMCTL,
            &SYSTEMD_STATUS_ARGS,
            "LoadState=not-found\nActiveState=inactive\nSubState=dead\nUnitFileState=enabled\n",
        ),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "disable", SYSTEMD_UNIT], ""),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"], ""),
    ]);
    let manager = detached_manager(runner);

    assert_eq!(manager.uninstall()?, DaemonStatus::NotInstalled);
    Ok(())
}

#[test]
fn uninstall_disables_and_removes_an_inactive_masked_definition() -> Result<(), DaemonError> {
    let root = test_root("uninstall-inactive-masked");
    let runner = ScriptedCommandRunner::new([
        CommandStep::success(
            SYSTEMCTL,
            &SYSTEMD_STATUS_ARGS,
            "LoadState=masked\nActiveState=inactive\nSubState=dead\nUnitFileState=masked\n",
        ),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "disable", SYSTEMD_UNIT], ""),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"], ""),
    ]);
    let manager = test_manager(&root, runner);
    write_atomic(&manager.unit_path, "installed")?;

    let status = manager.uninstall()?;

    assert_eq!(status, DaemonStatus::NotInstalled);
    assert!(!manager.unit_path.exists());
    Ok(())
}

#[test]
fn uninstall_stops_a_detached_running_unit_before_reloading() -> Result<(), DaemonError> {
    let runner = ScriptedCommandRunner::new([
        CommandStep::success(SYSTEMCTL, &SYSTEMD_STATUS_ARGS, DETACHED_RUNNING_STATUS),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "stop", SYSTEMD_UNIT], ""),
        CommandStep::success(SYSTEMCTL, &[SYSTEMD_USER_ARG, "daemon-reload"], ""),
    ]);
    let manager = detached_manager(runner);

    assert_eq!(manager.uninstall()?, DaemonStatus::NotInstalled);
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
        DaemonStatus::installed(DaemonState::Running, AutostartState::Reported("unknown"),)
    );
    Ok(())
}

#[test]
fn restart_of_installed_unit_preserves_autostart() -> Result<(), DaemonError> {
    let root = test_root("restart-sequence");
    let runner = ScriptedCommandRunner::new([
        CommandStep::success(
            SYSTEMCTL,
            &SYSTEMD_STATUS_ARGS,
            "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=disabled\nResult=success\n",
        ),
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
fn stop_preserves_the_manager_verdict_when_the_action_would_be_rejected() -> Result<(), DaemonError>
{
    for (status_output, expected) in [
        (NOT_INSTALLED_STATUS, DaemonStatus::NotInstalled),
        (
            "LoadState=masked\nActiveState=inactive\nSubState=dead\nUnitFileState=masked\n",
            DaemonStatus::installed(
                DaemonState::Reported {
                    status: "unavailable",
                    detail: None,
                },
                AutostartState::Reported("unavailable"),
            ),
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
fn stop_rejects_an_active_masked_unit_without_issuing_a_manager_action() {
    let runner = ScriptedCommandRunner::new([CommandStep::success(
        SYSTEMCTL,
        &SYSTEMD_STATUS_ARGS,
        "LoadState=masked\nActiveState=active\nSubState=running\nUnitFileState=masked\n",
    )]);
    let manager = detached_manager(runner);

    let result = manager.stop();

    assert!(matches!(
        result,
        Err(DaemonError::Systemd(SystemdError::UnitUnavailable))
    ));
}

#[test]
fn uninstall_rejects_an_active_masked_unit_without_issuing_a_manager_action() {
    let runner = ScriptedCommandRunner::new([CommandStep::success(
        SYSTEMCTL,
        &SYSTEMD_STATUS_ARGS,
        "LoadState=masked\nActiveState=active\nSubState=running\nUnitFileState=masked\n",
    )]);
    let manager = detached_manager(runner);

    let result = manager.uninstall();

    assert!(matches!(
        result,
        Err(DaemonError::Systemd(SystemdError::UnitUnavailable))
    ));
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
        DaemonStatus::installed(DaemonState::Stopped, AutostartState::Reported("unknown"),)
    );
    assert_eq!(
        manager.stop()?,
        DaemonStatus::installed(DaemonState::Stopped, AutostartState::Reported("unknown"),)
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
fn start_rejects_a_not_installed_unit() {
    let runner = ScriptedCommandRunner::new([CommandStep::success(
        SYSTEMCTL,
        &SYSTEMD_STATUS_ARGS,
        NOT_INSTALLED_STATUS,
    )]);
    let manager = detached_manager(runner);

    let result = manager.start();

    assert!(matches!(
        result,
        Err(DaemonError::ServiceNotInstalled { .. })
    ));
}

#[test]
fn restart_rejects_a_runtime_mask_even_when_the_local_definition_exists() -> Result<(), DaemonError>
{
    let root = test_root("restart-runtime-mask");
    let runner = ScriptedCommandRunner::new([CommandStep::success(
        SYSTEMCTL,
        &SYSTEMD_STATUS_ARGS,
        "LoadState=masked\nActiveState=inactive\nSubState=dead\nUnitFileState=masked\n",
    )]);
    let manager = test_manager(&root, runner);
    write_atomic(&manager.unit_path, "installed")?;

    let result = manager.restart();
    let rendered = result.as_ref().err().map(ToString::to_string);

    assert!(matches!(
        result,
        Err(DaemonError::Systemd(SystemdError::UnitUnavailable))
    ));
    assert_eq!(
        rendered.as_deref(),
        Some(
            "the systemd user unit is unavailable; inspect its load state and repair or unmask it before changing it"
        )
    );
    Ok(())
}
