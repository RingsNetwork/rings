//! Proves launchd commands preserve action provenance, autostart, and installed-state evidence.

use super::*;
use crate::daemon::report_started;
use crate::daemon::DaemonTransition;
#[test]
fn start_rejects_invalid_definition_before_creating_directories() -> Result<(), DaemonError> {
    let root = test_root("start-invalid-definition");
    let manager = scripted_manager(&root, std::iter::empty::<CommandStep>());
    let mut spec = service_spec(&LogLevel::Warn, &RuntimeFlavor::CurrentThread)?;
    spec.working_directory = "/tmp/rings\u{b}daemon".to_owned();

    let result = manager.start(&spec);

    assert!(matches!(
        result,
        Err(DaemonError::Launchd(LaunchdError::Definition(
            LaunchdDefinitionError::XmlIncompatibleValue { .. }
        )))
    ));
    assert!(!root.exists());
    Ok(())
}

#[test]
fn start_waits_for_bootout_then_bootstraps_without_kickstart() -> Result<(), DaemonError> {
    let root = test_root("start-sequence");
    let domain = TEST_DOMAIN;
    let target = test_target();
    let definition = launchd_definition_path(&root);
    let definition_text = path_text(&definition)?;
    let manager = scripted_manager(&root, [
        loaded_service(&target, "state = running\n"),
        CommandStep::success(LAUNCHCTL, &["bootout", &target], ""),
        missing_service(&target),
        CommandStep::success(LAUNCHCTL, &["enable", &target], ""),
        CommandStep::success(LAUNCHCTL, &["bootstrap", domain, &definition_text], ""),
        loaded_service(&target, "state = running\n"),
        enabled_autostart(domain),
    ]);
    let spec = service_spec(&LogLevel::Info, &RuntimeFlavor::MultiThread)?;

    let status = manager.start(&spec)?;

    assert!(manager.definition_path.is_file());
    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Running, AutostartState::Enabled)
    );
    Ok(())
}

#[test]
fn restart_reads_disabled_autostart_once_and_suppresses_action_signal() -> Result<(), DaemonError> {
    let root = test_root("restart-sequence");
    let domain = TEST_DOMAIN;
    let target = test_target();
    let mut steps = vec![
        loaded_service(&target, "state = running\nruns = 3\n"),
        CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
    ];
    fill_poll_budget(&mut steps, 0, || {
        loaded_service(
            &target,
            "state = throttled\nruns = 4\nlast terminating signal = Terminated: 15\n",
        )
    });
    steps.push(disabled_autostart(domain));
    let manager = scripted_manager(&root, steps);
    install_test_definition(&root)?;

    let status = manager.restart()?;
    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Restarting(None), AutostartState::Disabled)
    );
    assert!(matches!(
        report_started(&manager, status),
        Err(DaemonError::ServiceDidNotStart { .. })
    ));
    Ok(())
}

#[test]
fn healthy_restart_waits_past_an_action_translated_exit_code() -> Result<(), DaemonError> {
    let root = test_root("restart-current-failure");
    let target = test_target();
    let mut steps = vec![
        loaded_service(&target, "state = running\nruns = 4\n"),
        CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
    ];
    steps.push(loaded_service(
        &target,
        "state = spawn scheduled\nruns = 5\nlast exit code = 1\n",
    ));
    steps.push(loaded_service(&target, "state = running\nruns = 5\n"));
    steps.push(enabled_autostart(TEST_DOMAIN));
    let manager = scripted_manager(&root, steps);
    install_test_definition(&root)?;

    let status = manager.restart()?;
    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Running, AutostartState::Enabled)
    );
    report_started(&manager, status)
}

#[test]
fn healthy_restart_waits_past_an_exited_action_record() -> Result<(), DaemonError> {
    let root = test_root("restart-exited-action-record");
    let target = test_target();
    let manager = scripted_manager(&root, [
        loaded_service(&target, "state = running\nruns = 3\n"),
        CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
        loaded_service(&target, "state = exited\nruns = 4\nlast exit code = 1\n"),
        loaded_service(&target, "state = running\nruns = 4\n"),
        enabled_autostart(TEST_DOMAIN),
    ]);
    install_test_definition(&root)?;

    let status = manager.restart()?;

    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Running, AutostartState::Enabled)
    );
    Ok(())
}

#[test]
fn restart_settles_on_an_attributable_clean_exit_under_keepalive() -> Result<(), DaemonError> {
    let root = test_root("restart-attributed-clean-exit");
    let target = test_target();
    let manager = scripted_manager(&root, [
        loaded_service(&target, "state = running\nruns = 3\n"),
        CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
        loaded_service(&target, "state = exited\nruns = 6\nlast exit code = 0\n"),
        enabled_autostart(TEST_DOMAIN),
    ]);
    install_test_definition(&root)?;

    let status = manager.restart()?;

    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Stopped, AutostartState::Enabled)
    );
    Ok(())
}

#[test]
fn restart_keeps_polling_after_a_clean_exit_without_keepalive_evidence() -> Result<(), DaemonError>
{
    let root = test_root("restart-clean-exit-unknown-policy");
    let target = test_target();
    let manager = scripted_manager(&root, [
        loaded_service_without_respawn_policy(&target, "state = running\nruns = 3\n"),
        CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
        loaded_service_without_respawn_policy(
            &target,
            "state = exited\nruns = 6\nlast exit code = 0\n",
        ),
        loaded_service_without_respawn_policy(&target, "state = running\nruns = 6\n"),
        enabled_autostart(TEST_DOMAIN),
    ]);
    install_test_definition(&root)?;

    let status = manager.restart()?;

    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Running, AutostartState::Enabled)
    );
    Ok(())
}

#[test]
fn restart_reports_signal_crash_after_sequence_advances() -> Result<(), DaemonError> {
    let root = test_root("restart-signal-failure");
    let target = test_target();
    let mut steps = vec![
        loaded_service(&target, "state = running\nruns = 3\n"),
        CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
    ];
    steps.push(loaded_service(
        &target,
        "state = spawn scheduled\nruns = 4\nlast terminating signal = Segmentation fault: 11\n",
    ));
    fill_poll_budget(&mut steps, 1, || {
        loaded_service(
            &target,
            "state = throttled\nruns = 4\nlast terminating signal = Segmentation fault: 11\n",
        )
    });
    steps.push(enabled_autostart(TEST_DOMAIN));
    let manager = scripted_manager(&root, steps);
    install_test_definition(&root)?;

    let status = manager.restart()?;
    let error = report_started(&manager, status);
    let expected = DaemonStatus::installed(
        DaemonState::Restarting(Some(DaemonFailure::described(
            "signal Segmentation fault: 11",
        ))),
        AutostartState::Enabled,
    );

    assert!(matches!(
        error,
        Err(DaemonError::ServiceDidNotStart { status }) if status == expected
    ));
    Ok(())
}

#[test]
fn restart_rejects_an_unloaded_service_without_a_definition() {
    let root = test_root("restart-not-installed");
    let target = test_target();
    let manager = scripted_manager(&root, [missing_service(&target)]);

    let result = manager.restart();

    assert!(matches!(
        result,
        Err(DaemonError::ServiceNotInstalled { .. })
    ));
}

#[test]
fn restart_without_a_sequence_baseline_uses_state_only() -> Result<(), DaemonError> {
    let root = test_root("restart-missing-runs");
    let target = test_target();
    let manager = scripted_manager(&root, [
        loaded_service(&target, "state = running\n"),
        CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
        loaded_service(&target, "state = spawn scheduled\nlast exit code = 9\n"),
        loaded_service(&target, "state = running\n"),
        enabled_autostart(TEST_DOMAIN),
    ]);
    install_test_definition(&root)?;

    let status = manager.restart()?;
    report_started(&manager, status)
}

#[test]
fn restart_without_a_sequence_baseline_reports_crash_loop_as_starting() -> Result<(), DaemonError> {
    let root = test_root("restart-missing-runs-crash-loop");
    let target = test_target();
    let crash_loop = "state = spawn scheduled\nlast exit code = 1\n";
    let mut steps = vec![
        loaded_service(&target, "state = waiting\n"),
        CommandStep::success(LAUNCHCTL, &["kickstart", "-k", &target], ""),
    ];
    fill_poll_budget(&mut steps, 0, || loaded_service(&target, crash_loop));
    steps.push(enabled_autostart(TEST_DOMAIN));
    let manager = scripted_manager(&root, steps);
    install_test_definition(&root)?;

    let status = manager.restart()?;
    let error = report_started(&manager, status);
    let expected = DaemonStatus::installed(
        DaemonState::Transitioning(DaemonTransition::named("starting")),
        AutostartState::Enabled,
    );

    assert!(matches!(
        error,
        Err(DaemonError::ServiceDidNotStart { status }) if status == expected
    ));
    Ok(())
}

#[test]
fn start_reporting_observes_autostart_once_after_lifecycle_settles() -> Result<(), DaemonError> {
    let root = test_root("start-reporting");
    let domain = TEST_DOMAIN;
    let target = test_target();
    let definition = launchd_definition_path(&root);
    let definition_text = path_text(&definition)?;
    let manager = scripted_manager(&root, [
        missing_service(&target),
        CommandStep::success(LAUNCHCTL, &["enable", &target], ""),
        CommandStep::success(LAUNCHCTL, &["bootstrap", domain, &definition_text], ""),
        loaded_service(&target, "state = spawn scheduled\nruns = 0\n"),
        loaded_service(&target, "state = running\n"),
        enabled_autostart(domain),
    ]);
    let spec = service_spec(&LogLevel::Info, &RuntimeFlavor::MultiThread)?;

    let status = manager.start(&spec)?;
    report_started(&manager, status)
}

#[test]
fn start_reporting_returns_throttled_after_the_poll_budget() -> Result<(), DaemonError> {
    let root = test_root("start-reporting-failure");
    let domain = TEST_DOMAIN;
    let target = test_target();
    let definition = launchd_definition_path(&root);
    let definition_text = path_text(&definition)?;
    let mut steps = vec![
        missing_service(&target),
        CommandStep::success(LAUNCHCTL, &["enable", &target], ""),
        CommandStep::success(LAUNCHCTL, &["bootstrap", domain, &definition_text], ""),
    ];
    fill_poll_budget(&mut steps, 0, || {
        loaded_service(&target, "state = throttled\nlast exit code = 78\n")
    });
    steps.push(enabled_autostart(domain));
    let manager = scripted_manager(&root, steps);
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
fn start_waits_past_a_scheduled_retry_until_running() -> Result<(), DaemonError> {
    let root = test_root("start-reporting-signal-failure");
    let domain = TEST_DOMAIN;
    let target = test_target();
    let definition = launchd_definition_path(&root);
    let definition_text = path_text(&definition)?;
    let manager = scripted_manager(&root, [
        missing_service(&target),
        CommandStep::success(LAUNCHCTL, &["enable", &target], ""),
        CommandStep::success(LAUNCHCTL, &["bootstrap", domain, &definition_text], ""),
        loaded_service(
            &target,
            "state = spawn scheduled\nruns = 1\nlast terminating signal = Segmentation fault: 11\n",
        ),
        loaded_service(&target, "state = running\nruns = 1\n"),
        enabled_autostart(domain),
    ]);
    let spec = service_spec(&LogLevel::Info, &RuntimeFlavor::MultiThread)?;

    let status = manager.start(&spec)?;
    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Running, AutostartState::Enabled)
    );
    report_started(&manager, status)
}

#[test]
fn status_preserves_unexpected_launchctl_failures() {
    let root = test_root("status-failure");
    let target = test_target();
    let manager = scripted_manager(&root, [CommandStep::failure(
        LAUNCHCTL,
        &["print", &target],
        112,
        "Could not find specified domain",
    )]);

    let result = manager.observe();

    let failure_detail = match result {
        Err(DaemonError::CommandFailed(failure)) => failure.detail,
        _ => None,
    };
    assert_eq!(
        failure_detail.as_deref(),
        Some("Could not find specified domain")
    );
}

#[test]
fn status_reports_an_external_signal_crash() -> Result<(), DaemonError> {
    let root = test_root("status-signal-failure");
    let domain = TEST_DOMAIN;
    let target = test_target();
    install_test_definition(&root)?;
    let manager = scripted_manager(&root, [
        loaded_service(
            &target,
            "state = spawn scheduled\nruns = 7\nlast terminating signal = Bus error: 10\n",
        ),
        enabled_autostart(domain),
    ]);

    let status = manager.observe()?;

    assert_eq!(
        status,
        DaemonStatus::installed(
            DaemonState::Restarting(Some(DaemonFailure::described("signal Bus error: 10"))),
            AutostartState::Enabled,
        )
    );
    Ok(())
}

#[test]
fn status_maps_only_launchd_service_not_found_to_not_installed() -> Result<(), DaemonError> {
    let root = test_root("status-not-found");
    let target = test_target();
    let manager = scripted_manager(&root, [missing_service(&target)]);

    let status = manager.observe()?;

    assert_eq!(status, DaemonStatus::NotInstalled);
    Ok(())
}

#[test]
fn loaded_job_without_a_plist_still_queries_manager_autostart() -> Result<(), DaemonError> {
    let root = test_root("status-loaded-without-plist");
    let domain = TEST_DOMAIN;
    let target = test_target();
    let manager = scripted_manager(&root, [
        loaded_service(&target, "state = running\n"),
        enabled_autostart(domain),
    ]);

    let status = manager.observe()?;

    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Running, AutostartState::Enabled)
    );
    Ok(())
}
