use super::*;

#[test]
fn restart_bootstraps_an_installed_but_unloaded_service_without_enabling_autostart(
) -> Result<(), DaemonError> {
    let root = test_root("restart-bootstrap-sequence");
    let domain = TEST_DOMAIN;
    let target = TEST_TARGET.to_owned();
    let definition = install_test_definition(&root)?;
    let definition_text = path_text(&definition)?;
    let runner = ScriptedCommandRunner::new([
        CommandStep::failure(
            LAUNCHCTL,
            &["print", &target],
            LAUNCHD_SERVICE_NOT_FOUND,
            "Could not find specified service",
        ),
        CommandStep::failure(
            LAUNCHCTL,
            &["bootstrap", domain, &definition_text],
            LAUNCHD_BOOTSTRAP_DISABLED,
            "Input/output error",
        ),
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
    let root = test_root("restart-enabled-unloaded");
    let domain = TEST_DOMAIN;
    let target = TEST_TARGET.to_owned();
    let definition = install_test_definition(&root)?;
    let definition_text = path_text(&definition)?;
    let runner = ScriptedCommandRunner::new([
        CommandStep::failure(
            LAUNCHCTL,
            &["print", &target],
            LAUNCHD_SERVICE_NOT_FOUND,
            "Could not find specified service",
        ),
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
fn failed_bootstrap_still_restores_disabled_autostart() -> Result<(), DaemonError> {
    let root = test_root("restart-bootstrap-failure");
    let domain = TEST_DOMAIN;
    let target = TEST_TARGET.to_owned();
    let definition = install_test_definition(&root)?;
    let definition_text = path_text(&definition)?;
    let runner = ScriptedCommandRunner::new([
        CommandStep::failure(
            LAUNCHCTL,
            &["print", &target],
            LAUNCHD_SERVICE_NOT_FOUND,
            "Could not find specified service",
        ),
        CommandStep::failure(
            LAUNCHCTL,
            &["bootstrap", domain, &definition_text],
            LAUNCHD_BOOTSTRAP_DISABLED,
            "Input/output error",
        ),
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
        Err(DaemonError::Launchd(LaunchdError::DisabledBootstrap {
            failure: RecoveryFailure::Primary(_),
        }))
    ));
    Ok(())
}

#[test]
fn successful_bootstrap_reports_restore_failure() -> Result<(), DaemonError> {
    let root = test_root("restart-restore-failure");
    let domain = TEST_DOMAIN;
    let target = TEST_TARGET.to_owned();
    let definition = install_test_definition(&root)?;
    let definition_text = path_text(&definition)?;
    let runner = ScriptedCommandRunner::new([
        CommandStep::failure(
            LAUNCHCTL,
            &["print", &target],
            LAUNCHD_SERVICE_NOT_FOUND,
            "Could not find specified service",
        ),
        CommandStep::failure(
            LAUNCHCTL,
            &["bootstrap", domain, &definition_text],
            LAUNCHD_BOOTSTRAP_DISABLED,
            "Input/output error",
        ),
        CommandStep::success(LAUNCHCTL, &["enable", &target], ""),
        CommandStep::success(LAUNCHCTL, &["bootstrap", domain, &definition_text], ""),
        CommandStep::failure(LAUNCHCTL, &["disable", &target], 7, "restore failed"),
    ]);
    let manager = test_manager(&root, runner);

    let result = manager.restart();

    assert!(matches!(
        result,
        Err(DaemonError::Launchd(LaunchdError::DisabledBootstrap {
            failure: RecoveryFailure::Recovery(_),
        }))
    ));
    Ok(())
}

#[test]
fn failed_bootstrap_and_restore_preserve_both_errors() -> Result<(), DaemonError> {
    let root = test_root("restart-bootstrap-and-restore-failure");
    let domain = TEST_DOMAIN;
    let target = TEST_TARGET.to_owned();
    let definition = install_test_definition(&root)?;
    let definition_text = path_text(&definition)?;
    let runner = ScriptedCommandRunner::new([
        CommandStep::failure(
            LAUNCHCTL,
            &["print", &target],
            LAUNCHD_SERVICE_NOT_FOUND,
            "Could not find specified service",
        ),
        CommandStep::failure(
            LAUNCHCTL,
            &["bootstrap", domain, &definition_text],
            LAUNCHD_BOOTSTRAP_DISABLED,
            "Input/output error",
        ),
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
        Err(DaemonError::Launchd(LaunchdError::DisabledBootstrap {
            failure: RecoveryFailure::Both { .. },
        }))
    ));
    Ok(())
}

#[test]
fn enable_failure_does_not_attempt_a_restore() -> Result<(), DaemonError> {
    let root = test_root("restart-enable-failure");
    let domain = TEST_DOMAIN;
    let target = TEST_TARGET.to_owned();
    let definition = install_test_definition(&root)?;
    let definition_text = path_text(&definition)?;
    let runner = ScriptedCommandRunner::new([
        CommandStep::failure(
            LAUNCHCTL,
            &["print", &target],
            LAUNCHD_SERVICE_NOT_FOUND,
            "Could not find specified service",
        ),
        CommandStep::failure(
            LAUNCHCTL,
            &["bootstrap", domain, &definition_text],
            LAUNCHD_BOOTSTRAP_DISABLED,
            "Input/output error",
        ),
        CommandStep::failure(LAUNCHCTL, &["enable", &target], 7, "enable failed"),
    ]);
    let manager = test_manager(&root, runner);

    let result = manager.restart();

    assert!(matches!(result, Err(DaemonError::CommandFailed(_))));
    Ok(())
}

#[test]
fn non_disabled_bootstrap_failure_does_not_mutate_autostart() -> Result<(), DaemonError> {
    let root = test_root("restart-non-disabled-bootstrap-failure");
    let domain = TEST_DOMAIN;
    let target = TEST_TARGET.to_owned();
    let definition = install_test_definition(&root)?;
    let definition_text = path_text(&definition)?;
    let runner = ScriptedCommandRunner::new([
        CommandStep::failure(
            LAUNCHCTL,
            &["print", &target],
            LAUNCHD_SERVICE_NOT_FOUND,
            "Could not find specified service",
        ),
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
