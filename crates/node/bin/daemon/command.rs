//! Defines the public daemon command surface independently from manager effects.

use clap::Args;
use clap::Subcommand;

use super::ConfigArgs;

#[derive(Subcommand, Debug)]
#[command(rename_all = "kebab-case")]
pub(crate) enum DaemonCommand {
    #[command(about = "Installs the user-level node service and enables login startup.")]
    Install(DaemonInstallCommand),
    #[command(about = "Stops and removes the user-level node service.")]
    Uninstall,
    #[command(about = "Starts the installed user-level node service.")]
    Start,
    #[command(about = "Stops the user-level node service without disabling login startup.")]
    Stop,
    #[command(about = "Shows the service-manager and login-startup state.")]
    Status,
    #[command(about = "Restarts the installed service without changing login startup.")]
    Restart,
}

#[derive(Args, Debug)]
pub(crate) struct DaemonInstallCommand {
    #[command(flatten)]
    config_args: ConfigArgs,
}

impl DaemonInstallCommand {
    pub(crate) fn config_path(&self) -> &str {
        &self.config_args.config
    }
}
