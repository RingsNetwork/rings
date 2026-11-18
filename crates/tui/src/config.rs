//! Configuration and credential loading for `rings-tui`.

use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use serde::Deserialize;

use crate::error::Result;
use crate::error::TuiError;

/// Default node config path used when `--config` is not overridden.
pub const DEFAULT_CONFIG_PATH: &str = "~/.rings/config.yaml";
/// Default internal JSON-RPC and status endpoint URL.
pub const DEFAULT_ENDPOINT_URL: &str = "http://127.0.0.1:50000";
/// Default API token filename next to the node config file.
pub const DEFAULT_API_TOKEN_FILE: &str = "api-token";
/// Default TUI polling interval in milliseconds.
pub const DEFAULT_REFRESH_MS: u64 = 2_000;
/// Minimum allowed refresh interval in milliseconds.
pub const MIN_REFRESH_MS: u64 = 250;

/// Command-line arguments for launching `rings-tui`.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "rings-tui",
    about = "Interactive TUI for monitoring, status inspection, and command execution on a Rings node",
    version,
    author
)]
pub struct TuiArgs {
    /// Path to the node YAML configuration file.
    #[arg(
        long,
        short = 'c',
        env = "RINGS_CONFIG",
        default_value = DEFAULT_CONFIG_PATH,
        help = "Rings node config file location"
    )]
    pub config: String,

    /// Override the internal API endpoint URL (e.g. `http://127.0.0.1:50000`).
    #[arg(
        long,
        short = 'u',
        env = "RINGS_ENDPOINT_URL",
        help = "Rings node internal API endpoint URL"
    )]
    pub endpoint_url: Option<String>,

    /// Override the API Bearer token file path.
    #[arg(
        long,
        env = "RINGS_API_TOKEN_PATH",
        help = "API Bearer token file guarding the local node's internal API"
    )]
    pub api_token_path: Option<String>,

    /// Explicit API Bearer token string (prefer `--api-token-path` to avoid shell history exposure).
    #[arg(
        long,
        env = "RINGS_API_TOKEN",
        help = "API Bearer token value (optional override)"
    )]
    pub api_token: Option<String>,

    /// Automatic status refresh interval in milliseconds.
    #[arg(
        long,
        env = "RINGS_TUI_REFRESH_MS",
        default_value_t = DEFAULT_REFRESH_MS,
        help = "Auto-refresh interval in milliseconds (minimum 250)"
    )]
    pub refresh_ms: u64,

    /// Run with an in-memory simulated Rings node for previewing the TUI without a live daemon.
    #[arg(
        long,
        env = "RINGS_TUI_DEMO",
        help = "Run against an in-memory simulated node with live telemetry"
    )]
    pub demo: bool,

    /// Render a non-interactive snapshot of the TUI views to stdout and exit.
    #[arg(
        long,
        help = "Render a non-interactive snapshot of the TUI views to stdout and exit"
    )]
    pub print_preview: bool,
}

impl Default for TuiArgs {
    fn default() -> Self {
        Self {
            config: DEFAULT_CONFIG_PATH.to_string(),
            endpoint_url: None,
            api_token_path: None,
            api_token: None,
            refresh_ms: DEFAULT_REFRESH_MS,
            demo: false,
            print_preview: false,
        }
    }
}

/// Origin of the active API Bearer credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// Pinned explicitly on the CLI or via `RINGS_API_TOKEN`; disk reload must not overwrite it.
    CliExplicit,
    /// Loaded from `token_path` on disk.
    TokenFile,
    /// No token was available at startup; disk reload may load `token_path` later.
    Missing,
}

/// Subset of `config.yaml` relevant to a local operator TUI client.
#[derive(Debug, Default, Clone, Deserialize)]
struct PartialFileConfig {
    #[serde(default)]
    network_id: Option<u32>,
    #[serde(default)]
    internal_api_port: Option<u16>,
    #[serde(default)]
    external_api_addr: Option<String>,
    #[serde(default)]
    endpoint_url: Option<String>,
    #[serde(default)]
    api_token_path: Option<String>,
    #[serde(default)]
    onion_http_proxy_addr: Option<String>,
    #[serde(default)]
    advertise_onion_relay: Option<bool>,
    #[serde(default)]
    advertise_onion_exit: Option<bool>,
}

/// Resolved connection and display configuration for a TUI session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTuiConfig {
    /// Expanded config file path.
    pub config_path: PathBuf,
    /// Whether the config file existed and was loaded from disk.
    pub config_loaded: bool,
    /// Target internal HTTP JSON-RPC and `/status` endpoint URL.
    pub endpoint_url: String,
    /// External HTTP endpoint address from config, if known.
    pub external_api_addr: Option<String>,
    /// Local Onion HTTP CONNECT proxy address from config, if configured.
    pub onion_http_proxy_addr: Option<String>,
    /// Network ID from config, if known.
    pub network_id: Option<u32>,
    /// Whether config enables onion relay advertisement.
    pub advertise_onion_relay: bool,
    /// Whether config enables onion exit advertisement.
    pub advertise_onion_exit: bool,
    /// Resolved token file path, if applicable.
    pub token_path: PathBuf,
    /// Loaded Bearer token secret, if available.
    pub api_token: Option<String>,
    /// Provenance of `api_token`, governing whether disk reloads are permitted.
    pub credential_source: CredentialSource,
    /// Diagnostic note when the config or token file could not be loaded yet.
    pub warning: Option<String>,
    /// Polling interval for background status updates.
    pub refresh_interval: Duration,
    /// Whether the session uses the built-in simulated demo node client.
    pub demo: bool,
}

impl ResolvedTuiConfig {
    /// Resolve [`TuiArgs`] against the filesystem without failing when the node has not been
    /// initialized yet, allowing the TUI to start and display actionable status diagnostics.
    pub fn from_args(args: &TuiArgs) -> Result<Self> {
        let config_path = expand_home(&args.config);
        let mut warnings = Vec::new();
        let (file_cfg, config_loaded) = match fs::read_to_string(&config_path) {
            Ok(raw) => match serde_saphyr::from_str::<PartialFileConfig>(&raw) {
                Ok(parsed) => (parsed, true),
                Err(err) => {
                    warnings.push(format!(
                        "Config parse warning ({}): {err}",
                        config_path.display()
                    ));
                    (PartialFileConfig::default(), false)
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                warnings.push(format!(
                    "Config file not found at {} (using defaults)",
                    config_path.display()
                ));
                (PartialFileConfig::default(), false)
            }
            Err(err) => {
                warnings.push(format!(
                    "Could not read config {}: {err}",
                    config_path.display()
                ));
                (PartialFileConfig::default(), false)
            }
        };

        let endpoint_url = args
            .endpoint_url
            .clone()
            .or(file_cfg.endpoint_url)
            .or_else(|| {
                file_cfg
                    .internal_api_port
                    .map(|port| format!("http://127.0.0.1:{port}"))
            })
            .unwrap_or_else(|| DEFAULT_ENDPOINT_URL.to_string());

        let configured_token_path = args
            .api_token_path
            .as_deref()
            .or(file_cfg.api_token_path.as_deref());
        let token_path = resolve_token_path(&config_path, configured_token_path);

        let (api_token, credential_source) = if let Some(explicit) = args
            .api_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            (Some(explicit.to_owned()), CredentialSource::CliExplicit)
        } else {
            match fs::read_to_string(&token_path) {
                Ok(contents) => {
                    let trimmed = contents.trim().to_owned();
                    if trimmed.is_empty() {
                        warnings.push(format!("API token file {} is empty", token_path.display()));
                        (None, CredentialSource::Missing)
                    } else {
                        (Some(trimmed), CredentialSource::TokenFile)
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    warnings.push(format!(
                        "API token file not found at {}",
                        token_path.display()
                    ));
                    (None, CredentialSource::Missing)
                }
                Err(err) => {
                    warnings.push(format!(
                        "Could not read API token {}: {err}",
                        token_path.display()
                    ));
                    (None, CredentialSource::Missing)
                }
            }
        };

        let refresh_ms = args.refresh_ms.max(MIN_REFRESH_MS);
        Ok(Self {
            config_path,
            config_loaded,
            endpoint_url: if args.demo {
                "demo://127.0.0.1:50000 (Simulated Node)".to_string()
            } else {
                endpoint_url
            },
            external_api_addr: file_cfg.external_api_addr,
            onion_http_proxy_addr: file_cfg.onion_http_proxy_addr,
            network_id: file_cfg.network_id.or(Some(1)),
            advertise_onion_relay: file_cfg.advertise_onion_relay.unwrap_or(args.demo),
            advertise_onion_exit: file_cfg.advertise_onion_exit.unwrap_or(args.demo),
            token_path,
            api_token,
            credential_source,
            warning: if args.demo {
                Some("Running in DEMO mode (simulated live telemetry)".to_string())
            } else {
                (!warnings.is_empty()).then(|| warnings.join(" | "))
            },
            refresh_interval: Duration::from_millis(refresh_ms),
            demo: args.demo,
        })
    }

    /// Read and validate a candidate API token from `token_path` without mutating `self` yet,
    /// refusing when the credential was explicitly pinned via `--api-token` or when the token
    /// contains control characters that cannot form a valid HTTP `Authorization` header.
    pub fn load_token_candidate(&self) -> Result<String> {
        if self.credential_source == CredentialSource::CliExplicit {
            return Err(TuiError::ApiToken {
                path: self.token_path.clone(),
                reason:
                    "API token is pinned via --api-token / RINGS_API_TOKEN; disk reload is disabled"
                        .to_string(),
            });
        }
        let contents = fs::read_to_string(&self.token_path).map_err(|err| TuiError::ApiToken {
            path: self.token_path.clone(),
            reason: err.to_string(),
        })?;
        let trimmed = contents.trim().to_owned();
        if trimmed.is_empty() {
            return Err(TuiError::ApiToken {
                path: self.token_path.clone(),
                reason: "token file is empty".to_string(),
            });
        }
        if trimmed.chars().any(char::is_control)
            || reqwest::header::HeaderValue::from_str(&format!("Bearer {trimmed}")).is_err()
        {
            return Err(TuiError::ApiToken {
                path: self.token_path.clone(),
                reason: "token contains invalid HTTP header or control characters".to_string(),
            });
        }
        Ok(trimmed)
    }

    /// Commit a validated token after the new client transport has been constructed.
    pub fn commit_reloaded_token(&mut self, token: String) {
        self.api_token = Some(token);
        self.credential_source = CredentialSource::TokenFile;
        self.warning = None;
    }
}

/// Expand a leading `~` or `~/` to the current user's `$HOME` directory.
pub fn expand_home(path: impl AsRef<Path>) -> PathBuf {
    let raw = path.as_ref().to_string_lossy();
    if raw == "~" {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home);
        }
    } else if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    path.as_ref().to_path_buf()
}

/// Resolve the API token path relative to `config_path`'s parent directory when relative.
pub fn resolve_token_path(config_path: &Path, configured_path: Option<&str>) -> PathBuf {
    let parent = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let Some(configured) = configured_path else {
        return parent.join(DEFAULT_API_TOKEN_FILE);
    };
    let expanded = expand_home(configured);
    if expanded.is_absolute() {
        expanded
    } else {
        parent.join(expanded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_config_and_relative_api_token() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.yaml");
        let token_path = dir.path().join("api-token");
        fs::write(
            &cfg_path,
            "network_id: 7\ninternal_api_port: 50123\nexternal_api_addr: 127.0.0.1:50124\nendpoint_url: http://127.0.0.1:50123\nadvertise_onion_relay: true\n",
        )
        .expect("write config");
        fs::write(&token_path, "0123456789abcdef0123456789abcdef\n").expect("write token");

        let args = TuiArgs {
            config: cfg_path.to_string_lossy().to_string(),
            endpoint_url: None,
            api_token_path: None,
            api_token: None,
            refresh_ms: 1000,
            demo: false,
            print_preview: false,
        };
        let resolved = ResolvedTuiConfig::from_args(&args).expect("resolve config");
        assert!(resolved.config_loaded);
        assert_eq!(resolved.endpoint_url, "http://127.0.0.1:50123");
        assert_eq!(
            resolved.external_api_addr.as_deref(),
            Some("127.0.0.1:50124")
        );
        assert_eq!(resolved.network_id, Some(7));
        assert!(resolved.advertise_onion_relay);
        assert!(!resolved.advertise_onion_exit);
        assert_eq!(
            resolved.api_token.as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(resolved.credential_source, CredentialSource::TokenFile);
        assert!(resolved.warning.is_none());
    }

    #[test]
    fn cli_explicit_token_refuses_disk_reload_override() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg_path = dir.path().join("config.yaml");
        let token_path = dir.path().join("api-token");
        fs::write(&token_path, "disk-token-0123456789abcdef0123456789\n").expect("write token");

        let args = TuiArgs {
            config: cfg_path.to_string_lossy().to_string(),
            endpoint_url: None,
            api_token_path: None,
            api_token: Some("cli-pinned-token-0123456789abcdef0123".to_string()),
            refresh_ms: 1000,
            demo: false,
            print_preview: false,
        };
        let resolved = ResolvedTuiConfig::from_args(&args).expect("resolve config");
        assert_eq!(resolved.credential_source, CredentialSource::CliExplicit);
        assert!(resolved.load_token_candidate().is_err());

        // Also verify that a token file with internal newline / control characters is rejected.
        fs::write(&token_path, "line1\nline2").expect("write multiline token");
        let file_args = TuiArgs {
            config: cfg_path.to_string_lossy().to_string(),
            endpoint_url: None,
            api_token_path: Some(token_path.to_string_lossy().to_string()),
            api_token: None,
            refresh_ms: 1000,
            demo: false,
            print_preview: false,
        };
        let file_resolved = ResolvedTuiConfig::from_args(&file_args).expect("resolve config");
        assert!(file_resolved.load_token_candidate().is_err());
    }

    #[test]
    fn missing_config_falls_back_gracefully_with_warning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nonexistent.yaml");
        let args = TuiArgs {
            config: missing.to_string_lossy().to_string(),
            endpoint_url: None,
            api_token_path: None,
            api_token: None,
            refresh_ms: 100,
            demo: false,
            print_preview: false,
        };
        let resolved = ResolvedTuiConfig::from_args(&args).expect("resolve fallback");
        assert!(!resolved.config_loaded);
        assert_eq!(resolved.endpoint_url, DEFAULT_ENDPOINT_URL);
        assert_eq!(resolved.credential_source, CredentialSource::Missing);
        assert_eq!(
            resolved.refresh_interval,
            Duration::from_millis(MIN_REFRESH_MS)
        );
        assert!(resolved.warning.is_some());
    }
}
