//! Verifies every launchd bootstrap retry preserves the observed disabled-autostart setting and
//! reports both retry and restoration failures without changing error provenance.

use super::*;

fn bootstrap_fixture(name: &str) -> Result<(TestRoot, &'static str, String, String), DaemonError> {
    let root = test_root(name);
    let target = test_target();
    let definition = install_test_definition(&root)?;
    let definition_text = path_text(&definition)?;
    Ok((root, TEST_DOMAIN, target, definition_text))
}

#[test]
fn restart_bootstraps_an_installed_but_unloaded_service_without_enabling_autostart(
) -> Result<(), DaemonError> {
    let (root, domain, target, definition_text) = bootstrap_fixture("restart-bootstrap-sequence")?;
    let runner = ScriptedCommandRunner::new([
        missing_service(&target),
        disabled_bootstrap(domain, &definition_text),
        disabled_autostart(domain),
        CommandStep::success(LAUNCHCTL, &["enable", &target], ""),
        CommandStep::success(LAUNCHCTL, &["bootstrap", domain, &definition_text], ""),
        CommandStep::success(LAUNCHCTL, &["disable", &target], ""),
        CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
        disabled_autostart(domain),
    ]);
    let manager = test_manager(&root, runner);

    let status = manager.restart()?;

    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Running, AutostartState::Disabled)
    );
    Ok(())
}

#[test]
fn enabled_unloaded_restart_does_not_mutate_autostart() -> Result<(), DaemonError> {
    let (root, domain, target, definition_text) = bootstrap_fixture("restart-enabled-unloaded")?;
    let runner = ScriptedCommandRunner::new([
        missing_service(&target),
        CommandStep::success(LAUNCHCTL, &["bootstrap", domain, &definition_text], ""),
        CommandStep::success(LAUNCHCTL, &["print", &target], "state = running\n"),
        enabled_autostart(domain),
    ]);
    let manager = test_manager(&root, runner);

    let status = manager.restart()?;

    assert_eq!(
        status,
        DaemonStatus::installed(DaemonState::Running, AutostartState::Enabled)
    );
    Ok(())
}

#[test]
fn exit_five_with_enabled_autostart_does_not_mutate_the_observed_state() -> Result<(), DaemonError>
{
    let (root, domain, target, definition_text) = bootstrap_fixture("restart-exit-five-enabled")?;
    let runner = ScriptedCommandRunner::new([
        missing_service(&target),
        disabled_bootstrap(domain, &definition_text),
        enabled_autostart(domain),
    ]);
    let manager = test_manager(&root, runner);

    let result = manager.restart();

    assert!(matches!(
        result,
        Err(DaemonError::Launchd(LaunchdError::BootstrapStateMismatch {
            observed: AutostartState::Enabled,
            bootstrap,
        })) if matches!(
            &*bootstrap,
            DaemonError::CommandFailed(failure)
                if failure.status.code() == Some(LAUNCHD_BOOTSTRAP_DISABLED)
        )
    ));
    Ok(())
}

#[test]
fn exit_five_with_unknown_autostart_does_not_mutate_manager_state() -> Result<(), DaemonError> {
    let (root, domain, target, definition_text) = bootstrap_fixture("restart-exit-five-unknown")?;
    let runner = ScriptedCommandRunner::new([
        missing_service(&target),
        disabled_bootstrap(domain, &definition_text),
        CommandStep::success(
            LAUNCHCTL,
            &["print-disabled", domain],
            "unrecognized launchctl output",
        ),
    ]);
    let manager = test_manager(&root, runner);

    let result = manager.restart();

    assert!(matches!(
        result,
        Err(DaemonError::Launchd(LaunchdError::BootstrapStateMismatch {
            observed: AutostartState::Unknown,
            bootstrap,
        })) if matches!(
            &*bootstrap,
            DaemonError::CommandFailed(failure)
                if failure.status.code() == Some(LAUNCHD_BOOTSTRAP_DISABLED)
        )
    ));
    Ok(())
}

#[test]
fn failed_bootstrap_still_restores_disabled_autostart() -> Result<(), DaemonError> {
    let (root, domain, target, definition_text) = bootstrap_fixture("restart-bootstrap-failure")?;
    let runner = ScriptedCommandRunner::new([
        missing_service(&target),
        disabled_bootstrap(domain, &definition_text),
        disabled_autostart(domain),
        CommandStep::success(LAUNCHCTL, &["enable", &target], ""),
        CommandStep::failure(
            LAUNCHCTL,
            &["bootstrap", domain, &definition_text],
            6,
            "bootstrap retry failed",
        ),
        CommandStep::success(LAUNCHCTL, &["disable", &target], ""),
    ]);
    let manager = test_manager(&root, runner);

    let result = manager.restart();

    assert!(matches!(
        result,
        Err(DaemonError::Launchd(LaunchdError::BootstrapRetry { .. }))
    ));
    Ok(())
}

#[test]
fn successful_bootstrap_reports_restore_failure() -> Result<(), DaemonError> {
    let (root, domain, target, definition_text) = bootstrap_fixture("restart-restore-failure")?;
    let runner = ScriptedCommandRunner::new([
        missing_service(&target),
        disabled_bootstrap(domain, &definition_text),
        disabled_autostart(domain),
        CommandStep::success(LAUNCHCTL, &["enable", &target], ""),
        CommandStep::success(LAUNCHCTL, &["bootstrap", domain, &definition_text], ""),
        CommandStep::failure(LAUNCHCTL, &["disable", &target], 7, "restore failed"),
    ]);
    let manager = test_manager(&root, runner);

    let result = manager.restart();

    assert!(result.as_ref().err().is_some_and(|error| error
        .to_string()
        .contains("the service was bootstrapped, but restoring disabled login autostart failed")));
    assert!(result.as_ref().err().is_some_and(|error| error
        .to_string()
        .contains("the running service may remain enabled at login")));
    assert!(matches!(
        result,
        Err(DaemonError::Launchd(LaunchdError::AutostartRestore { .. }))
    ));
    Ok(())
}

#[test]
fn failed_bootstrap_and_restore_preserve_both_errors() -> Result<(), DaemonError> {
    let (root, domain, target, definition_text) =
        bootstrap_fixture("restart-bootstrap-and-restore-failure")?;
    let runner = ScriptedCommandRunner::new([
        missing_service(&target),
        disabled_bootstrap(domain, &definition_text),
        disabled_autostart(domain),
        CommandStep::success(LAUNCHCTL, &["enable", &target], ""),
        CommandStep::failure(
            LAUNCHCTL,
            &["bootstrap", domain, &definition_text],
            6,
            "bootstrap retry failed",
        ),
        CommandStep::failure(LAUNCHCTL, &["disable", &target], 7, "restore failed"),
    ]);
    let manager = test_manager(&root, runner);

    let result = manager.restart();

    assert!(matches!(
        result,
        Err(DaemonError::Launchd(
            LaunchdError::BootstrapRetryAndRestore {
                failure: RecoveryFailure::Both { .. },
            }
        ))
    ));
    Ok(())
}

#[test]
fn enable_failure_does_not_attempt_a_restore() -> Result<(), DaemonError> {
    let (root, domain, target, definition_text) = bootstrap_fixture("restart-enable-failure")?;
    let runner = ScriptedCommandRunner::new([
        missing_service(&target),
        disabled_bootstrap(domain, &definition_text),
        disabled_autostart(domain),
        CommandStep::failure(LAUNCHCTL, &["enable", &target], 7, "enable failed"),
    ]);
    let manager = test_manager(&root, runner);

    let result = manager.restart();

    assert!(matches!(
        result,
        Err(DaemonError::Launchd(LaunchdError::BootstrapEnable {
            bootstrap: primary,
            enable: recovery,
        })) if matches!(
            &*primary,
            DaemonError::CommandFailed(failure)
                if failure.status.code() == Some(LAUNCHD_BOOTSTRAP_DISABLED)
        ) && matches!(
            &*recovery,
            DaemonError::CommandFailed(failure) if failure.status.code() == Some(7)
        )
    ));
    Ok(())
}

#[test]
fn failed_autostart_probe_preserves_the_bootstrap_failure() -> Result<(), DaemonError> {
    let (root, domain, target, definition_text) =
        bootstrap_fixture("restart-autostart-probe-failure")?;
    let runner = ScriptedCommandRunner::new([
        missing_service(&target),
        disabled_bootstrap(domain, &definition_text),
        CommandStep::failure(
            LAUNCHCTL,
            &["print-disabled", domain],
            9,
            "session unavailable",
        ),
    ]);
    let manager = test_manager(&root, runner);

    let result = manager.restart();

    assert!(matches!(
        result,
        Err(DaemonError::Launchd(LaunchdError::BootstrapStateProbe {
            bootstrap: primary,
            probe: recovery,
        })) if matches!(
            &*primary,
            DaemonError::CommandFailed(failure)
                if failure.status.code() == Some(LAUNCHD_BOOTSTRAP_DISABLED)
        ) && matches!(
            &*recovery,
            DaemonError::CommandFailed(failure) if failure.status.code() == Some(9)
        )
    ));
    Ok(())
}

#[test]
fn non_disabled_bootstrap_failure_does_not_mutate_autostart() -> Result<(), DaemonError> {
    let (root, domain, target, definition_text) =
        bootstrap_fixture("restart-non-disabled-bootstrap-failure")?;
    let runner = ScriptedCommandRunner::new([
        missing_service(&target),
        CommandStep::failure(
            LAUNCHCTL,
            &["bootstrap", domain, &definition_text],
            6,
            "bootstrap failed",
        ),
    ]);
    let manager = test_manager(&root, runner);

    let result = manager.restart();

    assert!(matches!(result, Err(DaemonError::CommandFailed(_))));
    Ok(())
}
