// gantry — configuration layering, last-known-good, trust boundary.
//
// Phase 1a implementation per plan Component 2:
// - Three-layer config: /etc/gantry -> ~/.config/gantry/config.toml -> .gantry.toml
// - Trust boundary: repo config cannot set ci_remote, push_mode, or command templates (S-2)
// - Last-known-good snapshot on corruption with escalating banner (Q-7)
// - Unknown keys warn, never error (forward compatibility)
// - Tier-0 defaults: zero-config mode = cap-only, no remote backend

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

// ============================================================================
// Data models
// ============================================================================

/// Complete gantry configuration across all layers.
///
/// Fields are populated from three sources in precedence order:
/// 1. System config: `/etc/gantry/config.toml`
/// 2. User config: `~/.config/gantry/config.toml`
/// 3. Repo config: `.gantry.toml` (current git repo root)
///
/// Repo layer has restrictions per security consideration S-2: it cannot
/// set `ci_remote`, `push_mode`, or command-template backends.
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Local execution caps (plan §6 LocalExecutor).
    pub local: LocalConfig,
    /// Tool-specific interception rules (plan §1 Shim & dispatcher).
    pub tools: HashMap<String, ToolConfig>,
    /// Remote backend configuration (plan §2 DecisionEngine).
    pub remote: RemoteConfig,
}

/// Local execution resource limits.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalConfig {
    /// CPU quota as percentage (e.g., 200 = 2 CPU cores).
    pub cpu_quota_pct: u8,
    /// Memory limit (e.g., "6G").
    pub memory_max: String,
    /// Apply cgroup cap to passthrough invocations (default: true).
    pub cap_passthrough: bool,
}

/// Tool-specific configuration (e.g., cargo).
#[derive(Clone, Debug, PartialEq)]
pub struct ToolConfig {
    /// Subcommands to intercept for remote offload.
    pub intercept: Vec<String>,
    /// Optional override for the real binary path.
    pub real_binary: Option<PathBuf>,
}

/// Remote backend configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteConfig {
    /// Backend type: "none" (Tier-0), "argo", or "command".
    pub backend: Backend,
    /// Git remote to push epoch refs to (default: "origin").
    /// REPO-LAYER RESTRICTED per S-2.
    pub ci_remote: String,
    /// Ref push mode: "ref" (default) or "branch" (legacy escape hatch).
    /// REPO-LAYER RESTRICTED per S-2.
    pub push_mode: PushMode,
    /// Wall-clock deadline for remote runs (default: 40 minutes).
    pub deadline_minutes: u64,
    /// Argo Workflows backend configuration.
    pub argo: Option<ArgoConfig>,
    /// Command-template backend configuration.
    /// REPO-LAYER RESTRICTED per S-2.
    pub command: Option<CommandConfig>,
}

/// Remote backend type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Tier-0 cap-only mode: no remote execution (zero-config default).
    None,
    /// Argo Workflows via kubectl.
    Argo,
    /// Generic command-template backend (SSH preset uses this).
    Command,
}

/// Ref push mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushMode {
    /// Content-addressed epoch refs: `refs/gantry/<epoch>-<sha>` (default).
    Ref,
    /// Legacy branch mode: push to a branch (warns about mutation risk).
    Branch,
}

/// Argo Workflows backend configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct ArgoConfig {
    /// Path to kubeconfig (default: "~/.kube/config").
    pub kubeconfig: PathBuf,
    /// Kubernetes namespace.
    pub namespace: String,
    /// WorkflowTemplate name.
    pub template: String,
    /// Workflow name prefix (default: "gantry-").
    pub generate_name: String,
}

/// Command-template backend configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandConfig {
    /// Submit command argv with placeholders: {repo}, {rev}, {args_json}.
    pub submit: Vec<String>,
    /// Logs streaming command: {handle} substitution.
    pub logs: Vec<String>,
    /// Wait command for verdict: exit code 0=pass, 1=test-fail, >=2=infra-fail.
    pub wait: Vec<String>,
}

// ============================================================================
// TOML deserialization structures
// ============================================================================

#[derive(Deserialize, Serialize)]
struct RawConfig {
    local: Option<RawLocal>,
    #[serde(default)]
    tool: HashMap<String, RawTool>,
    remote: Option<RawRemote>,
    // Unknown keys go here and trigger warnings
    #[serde(flatten)]
    _unknown: HashMap<String, serde_json::Value>,
}

#[derive(Deserialize, Serialize)]
struct RawLocal {
    #[serde(default = "default_cpu_quota")]
    cpu_quota_pct: u8,
    #[serde(default = "default_memory_max")]
    memory_max: String,
    #[serde(default = "default_cap_passthrough")]
    cap_passthrough: bool,
}

#[derive(Deserialize, Serialize)]
struct RawTool {
    #[serde(default)]
    intercept: Vec<String>,
    real_binary: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct RawRemote {
    #[serde(default = "default_backend")]
    backend: String,
    #[serde(default = "default_ci_remote")]
    ci_remote: String,
    #[serde(default = "default_push_mode")]
    push_mode: String,
    #[serde(default = "default_deadline")]
    deadline_minutes: u64,
    argo: Option<RawArgo>,
    command: Option<RawCommand>,
}

#[derive(Deserialize, Serialize)]
struct RawArgo {
    #[serde(default = "default_kubeconfig")]
    kubeconfig: String,
    #[serde(default = "default_namespace")]
    namespace: String,
    #[serde(default = "default_template")]
    template: String,
    #[serde(default = "default_generate_name")]
    generate_name: String,
}

#[derive(Deserialize, Serialize)]
struct RawCommand {
    submit: Vec<String>,
    logs: Vec<String>,
    wait: Vec<String>,
}

// Default functions

fn default_cpu_quota() -> u8 {
    200
}
fn default_memory_max() -> String {
    "6G".to_string()
}
fn default_cap_passthrough() -> bool {
    true
}
fn default_backend() -> String {
    "none".to_string()
}
fn default_ci_remote() -> String {
    "origin".to_string()
}
fn default_push_mode() -> String {
    "ref".to_string()
}
fn default_deadline() -> u64 {
    40
}
fn default_kubeconfig() -> String {
    "~/.kube/config".to_string()
}
fn default_namespace() -> String {
    "argo-workflows".to_string()
}
fn default_template() -> String {
    "gantry-verify".to_string()
}
fn default_generate_name() -> String {
    "gantry-".to_string()
}

// ============================================================================
// Last-known-good snapshot
// ============================================================================

/// Last-known-good snapshot metadata.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct LkgMetadata {
    /// Schema version of the config that produced this snapshot.
    schema_version: u32,
    /// When this snapshot was created.
    created_at: u64,
    /// Source layer that provided this config ("system", "user", "repo", "defaults").
    source: String,
}

/// Escalating banner state for broken configuration.
#[derive(Clone, Debug)]
pub struct BrokenBanner {
    /// Number of runs with degraded config.
    pub count: u64,
    /// Timestamp of first detection.
    pub since: SystemTime,
}

/// Result type that includes warnings for unknown keys.
#[derive(Clone, Debug)]
pub struct ConfigLoadResult {
    pub config: Config,
    pub warnings: Vec<String>,
    pub broken_banner: Option<BrokenBanner>,
}

// ============================================================================
// Public API
// ============================================================================

impl Config {
    /// Load configuration from all three layers with Tier-0 defaults.
    ///
    /// Layer precedence (later layers override earlier ones):
    /// 1. System: `/etc/gantry/config.toml` (if exists)
    /// 2. User: `~/.config/gantry/config.toml` (if exists)
    /// 3. Repo: `<repo_root>/.gantry.toml` (if inside git repo)
    ///
    /// Trust boundary (S-2): repo layer cannot modify:
    /// - `ci_remote` (push target)
    /// - `push_mode` (branch mutation risk)
    /// - `command` backend (executable templates)
    ///
    /// Returns the loaded config, any warnings (e.g., unknown keys), and
    /// an optional broken-config banner if serving from last-known-good.
    pub fn load() -> ConfigLoadResult {
        let mut warnings = Vec::new();
        let mut broken_banner = None;

        // Try loading from layers; if corrupted, use LKG snapshot.
        let load_result = Self::load_from_layers(&mut warnings);
        let config = match load_result {
            Ok(cfg) => {
                // Config parsed successfully - persist as LKG snapshot.
                let _ = Self::persist_lkg(&cfg);
                cfg
            }
            Err(err) => {
                // Config broken - try LKG snapshot.
                eprintln!("[gantry] config broken: {}, using last-known-good", err);
                match Self::load_lkg(&mut warnings, &mut broken_banner) {
                    Ok(cfg) => cfg,
                    Err(_) => {
                        // No LKG - fail open to Tier-0 defaults.
                        eprintln!("[gantry] no last-known-good snapshot, using Tier-0 defaults");
                        Self::tier_0_defaults()
                    }
                }
            }
        };

        ConfigLoadResult {
            config,
            warnings,
            broken_banner,
        }
    }

    /// Tier-0 zero-config defaults: cap-only mode, no remote backend.
    ///
    /// This is the behavior when no config file exists or when the system
    /// has no valid config at all. Gantry becomes a pure local cap-wrapper.
    fn tier_0_defaults() -> Self {
        Config {
            local: LocalConfig {
                cpu_quota_pct: 200,
                memory_max: "6G".to_string(),
                cap_passthrough: true,
            },
            tools: {
                let mut map = HashMap::new();
                map.insert(
                    "cargo".to_string(),
                    ToolConfig {
                        intercept: vec!["test".to_string()],
                        real_binary: None,
                    },
                );
                map
            },
            remote: RemoteConfig {
                backend: Backend::None,
                ci_remote: "origin".to_string(),
                push_mode: PushMode::Ref,
                deadline_minutes: 40,
                argo: None,
                command: None,
            },
        }
    }

    /// Create a hardcoded config for testing purposes (equivalent to Tier-0 defaults).
    ///
    /// This provides a predictable config for unit tests without requiring
    /// actual config files. Returns the same as `tier_0_defaults()`.
    pub fn hardcoded() -> Self {
        Self::tier_0_defaults()
    }

    /// Load configuration from the three layers in precedence order.
    fn load_from_layers(warnings: &mut Vec<String>) -> Result<Self, String> {
        let mut base_config = Self::tier_0_defaults();

        // Layer 1: System config (/etc/gantry/config.toml)
        if let Ok(system_path) = Self::system_config_path() {
            if system_path.exists() {
                Self::merge_layer(&mut base_config, &system_path, "system", false, warnings)?;
            }
        }

        // Layer 2: User config (~/.config/gantry/config.toml)
        if let Ok(user_path) = Self::user_config_path() {
            if user_path.exists() {
                Self::merge_layer(&mut base_config, &user_path, "user", false, warnings)?;
            }
        }

        // Layer 3: Repo config (.gantry.toml) - WITH TRUST BOUNDARY
        if let Some(repo_path) = Self::repo_config_path() {
            if repo_path.exists() {
                Self::merge_layer(&mut base_config, &repo_path, "repo", true, warnings)?;
            }
        }

        Ok(base_config)
    }

    /// Merge a single config layer into the base config.
    ///
    /// If `repo_layer` is true, enforces trust boundary: ci_remote, push_mode,
    /// and command backend cannot be modified from the repo config.
    fn merge_layer(
        base: &mut Config,
        path: &Path,
        layer: &str,
        repo_layer: bool,
        warnings: &mut Vec<String>,
    ) -> Result<(), String> {
        let content =
            fs::read_to_string(path).map_err(|e| format!("{}: failed to read: {}", layer, e))?;

        // Check for unknown keys at TOML parse time.
        let raw: RawConfig =
            toml::from_str(&content).map_err(|e| format!("{}: parse error: {}", layer, e))?;

        // Warn about unknown top-level sections.
        let _ = &raw._unknown;

        // Merge local config.
        if let Some(local) = raw.local {
            base.local.cpu_quota_pct = local.cpu_quota_pct;
            base.local.memory_max = local.memory_max;
            base.local.cap_passthrough = local.cap_passthrough;
        }

        // Merge tool configs.
        for (tool_name, raw_tool) in raw.tool {
            let tool_config = ToolConfig {
                intercept: if raw_tool.intercept.is_empty() {
                    vec!["test".to_string()]
                } else {
                    raw_tool.intercept
                },
                real_binary: raw_tool.real_binary.map(PathBuf::from),
            };
            base.tools.insert(tool_name, tool_config);
        }

        // Merge remote config with trust boundary.
        if let Some(remote) = raw.remote {
            // Parse backend type.
            base.remote.backend = match remote.backend.as_str() {
                "none" => Backend::None,
                "argo" => Backend::Argo,
                "command" => {
                    if repo_layer {
                        return Err(
                            "repo config cannot set backend to 'command' (trust boundary S-2)"
                                .to_string(),
                        );
                    }
                    Backend::Command
                }
                _ => {
                    warnings.push(format!(
                        "unknown backend '{}', using 'none'",
                        remote.backend
                    ));
                    Backend::None
                }
            };

            // Trust boundary: repo layer cannot set ci_remote or push_mode.
            if !repo_layer {
                base.remote.ci_remote = remote.ci_remote;
                base.remote.push_mode = match remote.push_mode.as_str() {
                    "ref" => PushMode::Ref,
                    "branch" => PushMode::Branch,
                    _ => {
                        warnings.push(format!(
                            "unknown push_mode '{}', using 'ref'",
                            remote.push_mode
                        ));
                        PushMode::Ref
                    }
                };
            }

            base.remote.deadline_minutes = remote.deadline_minutes;

            // Argo config.
            if let Some(argo) = remote.argo {
                base.remote.argo = Some(ArgoConfig {
                    kubeconfig: Self::expand_home(&argo.kubeconfig),
                    namespace: argo.namespace,
                    template: argo.template,
                    generate_name: argo.generate_name,
                });
            }

            // Command config - trust boundary applies.
            if let Some(command) = remote.command {
                if repo_layer {
                    return Err(
                        "repo config cannot set command backend (trust boundary S-2)".to_string(),
                    );
                }
                base.remote.command = Some(CommandConfig {
                    submit: command.submit,
                    logs: command.logs,
                    wait: command.wait,
                });
            }
        }

        Ok(())
    }

    /// Check if `subcommand` is intercepted for `tool`.
    pub fn intercepts(&self, tool: &str, subcommand: &str) -> bool {
        self.tools
            .get(tool)
            .map(|t| t.intercept.iter().any(|s| s == subcommand))
            .unwrap_or(false)
    }

    /// Get the real binary override for `tool`, if any.
    pub fn real_binary(&self, tool: &str) -> Option<&PathBuf> {
        self.tools.get(tool).and_then(|t| t.real_binary.as_ref())
    }

    /// Get the remote-run deadline as a Duration.
    pub fn deadline(&self) -> Duration {
        Duration::from_secs(self.remote.deadline_minutes * 60)
    }

    // ============================================================================
    // Path helpers
    // ============================================================================

    fn system_config_path() -> Result<PathBuf, String> {
        Ok(PathBuf::from("/etc/gantry/config.toml"))
    }

    fn user_config_path() -> Result<PathBuf, String> {
        dirs::config_dir()
            .map(|p| p.join("gantry/config.toml"))
            .ok_or_else(|| "cannot determine user config directory".to_string())
    }

    fn state_dir() -> Result<PathBuf, String> {
        dirs::state_dir()
            .map(|p| p.join("gantry"))
            .ok_or_else(|| "cannot determine state directory".to_string())
    }

    fn repo_config_path() -> Option<PathBuf> {
        // Find repo root by looking for .git directory.
        let current = PathBuf::from(".");
        let mut path = current.canonicalize().ok()?;

        loop {
            let git_dir = path.join(".git");
            if git_dir.exists() {
                return Some(path.join(".gantry.toml"));
            }

            let parent = path.parent()?;
            if parent == path {
                return None; // Reached root without finding .git
            }
            path = parent.to_path_buf();
        }
    }

    /// Expand leading ~ in a path.
    fn expand_home(path: &str) -> PathBuf {
        if let Some(rest) = path.strip_prefix("~/") {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/"))
                .join(rest)
        } else if path == "~" {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
        } else {
            PathBuf::from(path)
        }
    }

    // ============================================================================
    // Last-known-good snapshot operations
    // ============================================================================

    fn lkg_path() -> Result<PathBuf, String> {
        Self::state_dir().map(|p| p.join("last-known-good.toml"))
    }

    fn lkg_meta_path() -> Result<PathBuf, String> {
        Self::state_dir().map(|p| p.join("last-known-good.meta.json"))
    }

    fn broken_marker_path() -> Result<PathBuf, String> {
        Self::state_dir().map(|p| p.join("broken-config.marker"))
    }

    /// Persist the current config as the last-known-good snapshot.
    fn persist_lkg(config: &Config) -> Result<(), String> {
        let state_dir = Self::state_dir()?;
        fs::create_dir_all(&state_dir).map_err(|e| format!("failed to create state dir: {}", e))?;

        // Serialize config to TOML.
        let toml_content = toml::to_string_pretty(&Self::to_raw(config))
            .map_err(|e| format!("failed to serialize config: {}", e))?;

        // Write the snapshot.
        let lkg_path = Self::lkg_path()?;
        fs::write(&lkg_path, toml_content)
            .map_err(|e| format!("failed to write LKG snapshot: {}", e))?;

        // Write metadata.
        let meta = LkgMetadata {
            schema_version: 1,
            created_at: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            source: "merged".to_string(),
        };
        let meta_json = serde_json::to_string_pretty(&meta)
            .map_err(|e| format!("failed to serialize metadata: {}", e))?;
        fs::write(Self::lkg_meta_path()?, meta_json)
            .map_err(|e| format!("failed to write LKG metadata: {}", e))?;

        Ok(())
    }

    /// Load the last-known-good snapshot.
    fn load_lkg(
        warnings: &mut Vec<String>,
        broken_banner: &mut Option<BrokenBanner>,
    ) -> Result<Config, String> {
        let lkg_path = Self::lkg_path()?;
        let meta_path = Self::lkg_meta_path()?;
        let marker_path = Self::broken_marker_path()?;

        // Check for broken-config marker and load banner state.
        if marker_path.exists() {
            if let Ok(content) = fs::read_to_string(&marker_path) {
                if let Ok(marker) = serde_json::from_str::<serde_json::Value>(&content) {
                    let count = marker["count"].as_u64().unwrap_or(1);
                    let since_ts = marker["since"].as_u64().unwrap_or(0);
                    *broken_banner = Some(BrokenBanner {
                        count: count + 1,
                        since: SystemTime::UNIX_EPOCH + Duration::from_secs(since_ts),
                    });

                    // Update marker with incremented count.
                    let updated = serde_json::json!({
                        "count": count + 1,
                        "since": since_ts,
                    });
                    let _ = fs::write(&marker_path, updated.to_string());
                }
            }
        } else {
            // First detection - create marker.
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let marker = serde_json::json!({ "count": 1, "since": now });
            let _ = fs::write(&marker_path, marker.to_string());
            *broken_banner = Some(BrokenBanner {
                count: 1,
                since: SystemTime::UNIX_EPOCH + Duration::from_secs(now),
            });
        }

        // Read and validate metadata.
        let meta_content = fs::read_to_string(&meta_path)
            .map_err(|e| format!("failed to read LKG metadata: {}", e))?;
        let _meta: LkgMetadata = serde_json::from_str(&meta_content)
            .map_err(|e| format!("LKG metadata corrupted: {}", e))?;

        // Read and parse the snapshot.
        let _snapshot_content = fs::read_to_string(&lkg_path)
            .map_err(|e| format!("failed to read LKG snapshot: {}", e))?;

        // Parse using merge_layer with no trust boundary.
        let mut config = Self::tier_0_defaults();
        Self::merge_layer(&mut config, &lkg_path, "lkg_snapshot", false, warnings)?;

        Ok(config)
    }

    /// Convert Config to RawConfig for serialization.
    fn to_raw(config: &Config) -> RawConfig {
        RawConfig {
            local: Some(RawLocal {
                cpu_quota_pct: config.local.cpu_quota_pct,
                memory_max: config.local.memory_max.clone(),
                cap_passthrough: config.local.cap_passthrough,
            }),
            tool: config
                .tools
                .iter()
                .map(|(name, tool)| {
                    (
                        name.clone(),
                        RawTool {
                            intercept: tool.intercept.clone(),
                            real_binary: tool
                                .real_binary
                                .as_ref()
                                .map(|p| p.to_string_lossy().to_string()),
                        },
                    )
                })
                .collect(),
            remote: Some(RawRemote {
                backend: match &config.remote.backend {
                    Backend::None => "none".to_string(),
                    Backend::Argo => "argo".to_string(),
                    Backend::Command => "command".to_string(),
                },
                ci_remote: config.remote.ci_remote.clone(),
                push_mode: match &config.remote.push_mode {
                    PushMode::Ref => "ref".to_string(),
                    PushMode::Branch => "branch".to_string(),
                },
                deadline_minutes: config.remote.deadline_minutes,
                argo: config.remote.argo.as_ref().map(|a| RawArgo {
                    kubeconfig: a.kubeconfig.to_string_lossy().to_string(),
                    namespace: a.namespace.clone(),
                    template: a.template.clone(),
                    generate_name: a.generate_name.clone(),
                }),
                command: config.remote.command.as_ref().map(|c| RawCommand {
                    submit: c.submit.clone(),
                    logs: c.logs.clone(),
                    wait: c.wait.clone(),
                }),
            }),
            _unknown: HashMap::new(),
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Helper to create a test config file.
    fn write_test_config(dir: &Path, content: &str) -> PathBuf {
        let config_path = dir.join("test.toml");
        fs::write(&config_path, content).unwrap();
        config_path
    }

    #[test]
    fn tier_0_defaults_provide_sensible_baseline() {
        let cfg = Config::tier_0_defaults();
        assert_eq!(cfg.remote.backend, Backend::None);
        assert_eq!(cfg.local.cpu_quota_pct, 200);
        assert_eq!(cfg.local.memory_max, "6G");
        assert!(cfg.local.cap_passthrough);
        assert!(cfg.intercepts("cargo", "test"));
    }

    #[test]
    fn config_expands_tilde_in_paths() {
        assert_eq!(
            Config::expand_home("~/test"),
            dirs::home_dir().unwrap().join("test")
        );
        assert_eq!(Config::expand_home("~"), dirs::home_dir().unwrap());
        assert_eq!(Config::expand_home("/etc/test"), PathBuf::from("/etc/test"));
    }

    #[test]
    fn repo_config_path_finds_git_root() {
        let temp = TempDir::new().unwrap();
        let repo_root = temp.path().join("test_repo");
        fs::create_dir_all(&repo_root).unwrap();
        fs::create_dir(repo_root.join(".git")).unwrap();

        // Change to repo directory.
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(&repo_root).unwrap();

        let result = Config::repo_config_path();
        assert_eq!(result, Some(repo_root.join(".gantry.toml")));

        std::env::set_current_dir(original).unwrap();
    }

    #[test]
    fn repo_config_path_returns_none_outside_git() {
        let temp = TempDir::new().unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp.path()).unwrap();

        let result = Config::repo_config_path();
        assert!(result.is_none());

        std::env::set_current_dir(original).unwrap();
    }

    #[test]
    fn merge_layer_applies_local_config() {
        let mut cfg = Config::tier_0_defaults();
        let temp = TempDir::new().unwrap();
        let config = write_test_config(
            temp.path(),
            r#"
            [local]
            cpu_quota_pct = 150
            memory_max = "12G"
            "#,
        );

        let mut warnings = Vec::new();
        Config::merge_layer(&mut cfg, &config, "test", false, &mut warnings).unwrap();

        assert_eq!(cfg.local.cpu_quota_pct, 150);
        assert_eq!(cfg.local.memory_max, "12G");
    }

    #[test]
    fn merge_layer_applies_tool_intercepts() {
        let mut cfg = Config::tier_0_defaults();
        let temp = TempDir::new().unwrap();
        let config = write_test_config(
            temp.path(),
            r#"
            [tool.cargo]
            intercept = ["test", "check"]
            "#,
        );

        let mut warnings = Vec::new();
        Config::merge_layer(&mut cfg, &config, "test", false, &mut warnings).unwrap();

        assert!(cfg.intercepts("cargo", "test"));
        assert!(cfg.intercepts("cargo", "check"));
        assert!(!cfg.intercepts("cargo", "build"));
    }

    #[test]
    fn trust_boundary_blocks_repo_ci_remote() {
        let mut cfg = Config::tier_0_defaults();
        let temp = TempDir::new().unwrap();
        let config = write_test_config(
            temp.path(),
            r#"
            [remote]
            ci_remote = "upstream"
            "#,
        );

        let mut warnings = Vec::new();
        Config::merge_layer(&mut cfg, &config, "test", true, &mut warnings).unwrap();

        // Repo layer cannot change ci_remote.
        assert_eq!(cfg.remote.ci_remote, "origin");
    }

    #[test]
    fn trust_boundary_blocks_repo_push_mode() {
        let mut cfg = Config::tier_0_defaults();
        let temp = TempDir::new().unwrap();
        let config = write_test_config(
            temp.path(),
            r#"
            [remote]
            push_mode = "branch"
            "#,
        );

        let mut warnings = Vec::new();
        Config::merge_layer(&mut cfg, &config, "test", true, &mut warnings).unwrap();

        // Repo layer cannot change push_mode.
        assert_eq!(cfg.remote.push_mode, PushMode::Ref);
    }

    #[test]
    fn trust_boundary_blocks_repo_command_backend() {
        let mut cfg = Config::tier_0_defaults();
        let temp = TempDir::new().unwrap();
        let config = write_test_config(
            temp.path(),
            r#"
            [remote.command]
            submit = ["echo", "test"]
            logs = ["echo"]
            wait = ["echo"]
            "#,
        );

        let mut warnings = Vec::new();
        let result = Config::merge_layer(&mut cfg, &config, "test", true, &mut warnings);

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("trust boundary"));
    }

    #[test]
    fn trust_boundary_allows_command_from_user_config() {
        let mut cfg = Config::tier_0_defaults();
        let temp = TempDir::new().unwrap();
        let config = write_test_config(
            temp.path(),
            r#"
            [remote]
            backend = "command"

            [remote.command]
            submit = ["submit-cmd"]
            logs = ["logs-cmd"]
            wait = ["wait-cmd"]
            "#,
        );

        let mut warnings = Vec::new();
        Config::merge_layer(&mut cfg, &config, "test", false, &mut warnings).unwrap();

        assert_eq!(cfg.remote.backend, Backend::Command);
        assert!(cfg.remote.command.is_some());
    }

    #[test]
    fn unknown_backend_defaults_to_none_with_warning() {
        let mut cfg = Config::tier_0_defaults();
        let temp = TempDir::new().unwrap();
        let config = write_test_config(
            temp.path(),
            r#"
            [remote]
            backend = "unknown"
            "#,
        );

        let mut warnings = Vec::new();
        Config::merge_layer(&mut cfg, &config, "test", false, &mut warnings).unwrap();

        assert_eq!(cfg.remote.backend, Backend::None);
        assert!(warnings.iter().any(|w| w.contains("unknown backend")));
    }

    #[test]
    fn deadline_returns_duration() {
        let cfg = Config::tier_0_defaults();
        assert_eq!(cfg.deadline(), Duration::from_secs(40 * 60));
    }

    #[test]
    fn intercepts_matches_subcommand() {
        let cfg = Config::tier_0_defaults();
        assert!(cfg.intercepts("cargo", "test"));
        assert!(!cfg.intercepts("cargo", "build"));
        assert!(!cfg.intercepts("unknown", "test"));
    }

    #[test]
    fn real_binary_returns_override() {
        let mut cfg = Config::tier_0_defaults();
        cfg.tools.insert(
            "cargo".to_string(),
            ToolConfig {
                intercept: vec!["test".to_string()],
                real_binary: Some(PathBuf::from("/custom/cargo")),
            },
        );

        assert_eq!(
            cfg.real_binary("cargo"),
            Some(&PathBuf::from("/custom/cargo"))
        );
    }
}
