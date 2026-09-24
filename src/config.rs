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
pub struct GantryConfig {
    /// Local execution caps (plan §6 LocalExecutor).
    pub local: LocalConfig,
    /// Tool-specific interception rules (plan §1 Shim & dispatcher).
    pub tools: HashMap<String, ToolConfig>,
    /// Remote backend configuration (plan §2 DecisionEngine).
    pub remote: RemoteConfig,
}

/// Back-compat alias: this struct shipped as `Config`; the canonical name is
/// [`GantryConfig`], matching the plan and bead naming.
pub type Config = GantryConfig;

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

/// Configuration layer, in precedence order (later layers override earlier).
///
/// Serde representation is the lowercase layer name ("system", "user",
/// "repo", "defaults") so it round-trips through TOML and JSON config
/// metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigLayer {
    /// System config: `/etc/gantry/config.toml`.
    System,
    /// User config: `~/.config/gantry/config.toml`.
    User,
    /// Repo config: `.gantry.toml` (trust boundary S-2 applies).
    Repo,
    /// Built-in Tier-0 defaults (no file); also the provenance recorded for
    /// merged last-known-good snapshots.
    Defaults,
}

impl std::fmt::Display for ConfigLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            ConfigLayer::System => "system",
            ConfigLayer::User => "user",
            ConfigLayer::Repo => "repo",
            ConfigLayer::Defaults => "defaults",
        };
        f.write_str(name)
    }
}

/// Argo Workflows backend configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct ArgoConfig {
    /// Path to the kubectl binary (default: "kubectl", resolved via PATH).
    pub kubectl_path: String,
    /// Path to kubeconfig (default: "~/.kube/config").
    pub kubeconfig: PathBuf,
    /// Kubernetes namespace.
    pub namespace: String,
    /// WorkflowTemplate name.
    pub template: String,
    /// Workflow name prefix (default: "gantry-").
    pub generate_name: String,
    /// Builder image passed as the template's `builder-image` parameter
    /// (optional; omitted from the manifest when unset so the template
    /// default applies).
    pub builder_image: Option<String>,
    /// Base URL for Argo UI (optional, for describe() to return human-readable URLs).
    /// Example: "https://argo-ci.ardenone.com" or "http://localhost:8080"
    pub base_url: Option<String>,
}

impl Default for ArgoConfig {
    fn default() -> Self {
        ArgoConfig {
            kubectl_path: default_kubectl_path(),
            kubeconfig: PathBuf::from(default_kubeconfig()),
            namespace: default_namespace(),
            template: default_template(),
            generate_name: default_generate_name(),
            builder_image: None,
            base_url: None,
        }
    }
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

// Every raw struct carries an `unknown` flatten map: keys we do not recognize
// land there instead of failing the parse, and merge_layer turns them into
// warnings (forward compatibility — warn, never error). The map value is
// `toml::Value`, not `serde_json::Value`, so even a TOML datetime in an
// unknown key still buffers instead of erroring.
//
// Typed fields are Option so that merge can tell "key absent" (inherit the
// lower layer's value) from "key present" (override). This is what makes the
// merge key-granular: a layer that sets cpu_quota_pct must not reset the
// memory_max a lower layer already chose.

#[derive(Deserialize, Serialize)]
struct RawConfig {
    local: Option<RawLocal>,
    #[serde(default)]
    tool: HashMap<String, RawTool>,
    remote: Option<RawRemote>,
    #[serde(flatten)]
    unknown: HashMap<String, toml::Value>,
}

#[derive(Deserialize, Serialize)]
struct RawLocal {
    cpu_quota_pct: Option<u8>,
    memory_max: Option<String>,
    cap_passthrough: Option<bool>,
    #[serde(flatten)]
    unknown: HashMap<String, toml::Value>,
}

#[derive(Deserialize, Serialize)]
struct RawTool {
    intercept: Option<Vec<String>>,
    real_binary: Option<String>,
    #[serde(flatten)]
    unknown: HashMap<String, toml::Value>,
}

#[derive(Deserialize, Serialize)]
struct RawRemote {
    backend: Option<String>,
    ci_remote: Option<String>,
    push_mode: Option<String>,
    deadline_minutes: Option<u64>,
    argo: Option<RawArgo>,
    command: Option<RawCommand>,
    #[serde(flatten)]
    unknown: HashMap<String, toml::Value>,
}

#[derive(Deserialize, Serialize)]
struct RawArgo {
    kubectl_path: Option<String>,
    kubeconfig: Option<String>,
    namespace: Option<String>,
    template: Option<String>,
    generate_name: Option<String>,
    builder_image: Option<String>,
    base_url: Option<String>,
    #[serde(flatten)]
    unknown: HashMap<String, toml::Value>,
}

#[derive(Deserialize, Serialize)]
struct RawCommand {
    submit: Vec<String>,
    logs: Vec<String>,
    wait: Vec<String>,
    #[serde(flatten)]
    unknown: HashMap<String, toml::Value>,
}

// Default functions — the [remote.argo] baseline. Shared between serde-free
// construction (ArgoConfig::default) and key-granular layer merging.
fn default_kubectl_path() -> String {
    "kubectl".to_string()
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
    /// Source layer that provided this config.
    source: ConfigLayer,
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
    pub config: GantryConfig,
    pub warnings: Vec<String>,
    pub broken_banner: Option<BrokenBanner>,
}

// ============================================================================
// Public API
// ============================================================================

impl GantryConfig {
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
        // Resolve the real layer and state paths; a path that cannot be
        // determined is simply absent (state_dir absent disables the LKG
        // mechanism entirely — the fallback is then always Tier-0).
        let system = Self::system_config_path().ok();
        let user = Self::user_config_path().ok();
        let repo = Self::repo_config_path();
        let state_dir = Self::state_dir().ok();

        Self::load_with_paths(
            system.as_deref(),
            user.as_deref(),
            repo.as_deref(),
            state_dir.as_deref(),
        )
    }

    /// [`load`](Self::load) with every filesystem location supplied by the
    /// caller — the testable core of the last-known-good contract (Q-7).
    ///
    /// Like [`load_layers`](Self::load_layers) this touches no
    /// process-global state: `state_dir` holds the LKG snapshot and the
    /// broken-config marker. Failure posture is never silent and never
    /// blocking:
    ///
    /// - clean load → config served, snapshot refreshed, broken marker
    ///   cleared (a fix stops the banner and resets escalation);
    /// - broken config + usable snapshot → snapshot served with a
    ///   broken-config banner whose run count escalates;
    /// - broken config, no usable snapshot → Tier-0 defaults, banner still
    ///   shown, and every degradation reason on stderr.
    fn load_with_paths(
        system: Option<&Path>,
        user: Option<&Path>,
        repo: Option<&Path>,
        state_dir: Option<&Path>,
    ) -> ConfigLoadResult {
        let mut warnings = Vec::new();
        let mut broken_banner = None;

        // Try loading from layers; if corrupted, use LKG snapshot.
        let load_result = Self::load_layers(system, user, repo);
        let config = match load_result {
            Ok(result) => {
                warnings.extend(result.warnings);
                // Config parsed successfully — refresh the LKG snapshot and
                // retire the banner: the marker must not survive a fix, or a
                // later incident would inherit the old count and "since".
                if let Some(dir) = state_dir {
                    let _ = Self::persist_lkg_in(dir, &result.config);
                    Self::clear_broken_marker_in(dir);
                }
                result.config
            }
            Err(err) => {
                // Config broken - try LKG snapshot.
                eprintln!("[gantry] config broken: {}, using last-known-good", err);
                let lkg = state_dir.and_then(|dir| {
                    match Self::load_lkg_in(dir, &mut warnings, &mut broken_banner) {
                        Ok(cfg) => Some(cfg),
                        Err(e) => {
                            // The snapshot itself is missing or corrupt — say
                            // why instead of lumping it in with "no snapshot".
                            eprintln!("[gantry] last-known-good snapshot unusable: {}", e);
                            None
                        }
                    }
                });
                lkg.unwrap_or_else(|| {
                    // No LKG - fail open to Tier-0 defaults (still bannered —
                    // the marker was written by load_lkg_in).
                    eprintln!("[gantry] no usable last-known-good snapshot, using Tier-0 defaults");
                    Self::tier_0_defaults()
                })
            }
        };

        ConfigLoadResult {
            config,
            warnings,
            broken_banner,
        }
    }

    /// Merge the three config layers over the Tier-0 defaults — the actual
    /// layering/resolution logic, with every path supplied by the caller.
    ///
    /// This is [`load`](Self::load) minus process-global state: no `/etc`,
    /// no `$HOME`, no cwd walk, no last-known-good reads or writes. `None`
    /// means the layer is absent; a `Some` path that does not exist is
    /// skipped the same way. Tests drive this directly against fixture
    /// files to prove layering end-to-end.
    ///
    /// On success the [`ConfigLoadResult`] carries the merged config plus
    /// every warning (unknown keys, ignored trust-boundary keys). On
    /// failure — unreadable file, parse error, or a repo-layer trust-
    /// boundary violation — returns Err and the caller falls back
    /// (last-known-good, then Tier-0).
    pub fn load_layers(
        system: Option<&Path>,
        user: Option<&Path>,
        repo: Option<&Path>,
    ) -> Result<ConfigLoadResult, String> {
        let mut warnings = Vec::new();
        let mut config = Self::tier_0_defaults();

        // Later layers override earlier ones, key by key.
        for (layer, path) in [
            (ConfigLayer::System, system),
            (ConfigLayer::User, user),
            (ConfigLayer::Repo, repo),
        ] {
            let Some(path) = path else { continue };
            if !path.exists() {
                continue;
            }
            Self::merge_layer(&mut config, path, layer, &mut warnings)?;
        }

        Ok(ConfigLoadResult {
            config,
            warnings,
            broken_banner: None,
        })
    }

    /// Tier-0 zero-config defaults: cap-only mode, no remote backend.
    ///
    /// This is the behavior when no config file exists or when the system
    /// has no valid config at all. Gantry becomes a pure local cap-wrapper.
    fn tier_0_defaults() -> Self {
        GantryConfig {
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

    /// Merge a single config layer into the base config.
    ///
    /// The merge is key-granular: only keys the layer actually sets override
    /// the base; everything else inherits what a lower layer chose (or the
    /// Tier-0 default). If `layer` is [`ConfigLayer::Repo`], enforces the
    /// trust boundary: ci_remote, push_mode, and command templates cannot be
    /// modified from the repo config.
    fn merge_layer(
        base: &mut GantryConfig,
        path: &Path,
        layer: ConfigLayer,
        warnings: &mut Vec<String>,
    ) -> Result<(), String> {
        let repo_layer = layer == ConfigLayer::Repo;
        let content =
            fs::read_to_string(path).map_err(|e| format!("{}: failed to read: {}", layer, e))?;

        // Parse never rejects unknown keys; each struct's flatten map
        // captures them and they are reported below (warn, never error).
        let raw: RawConfig =
            toml::from_str(&content).map_err(|e| format!("{}: parse error: {}", layer, e))?;

        // Unknown keys warn at the top level and within every section.
        warn_unknown_keys(&raw.unknown, "", layer, warnings);
        if let Some(local) = &raw.local {
            warn_unknown_keys(&local.unknown, "local", layer, warnings);
        }
        for (name, tool) in &raw.tool {
            warn_unknown_keys(&tool.unknown, &format!("tool.{name}"), layer, warnings);
        }
        if let Some(remote) = &raw.remote {
            warn_unknown_keys(&remote.unknown, "remote", layer, warnings);
            if let Some(argo) = &remote.argo {
                warn_unknown_keys(&argo.unknown, "remote.argo", layer, warnings);
            }
            if let Some(command) = &remote.command {
                warn_unknown_keys(&command.unknown, "remote.command", layer, warnings);
            }
        }

        // Merge local config, key by key.
        if let Some(local) = raw.local {
            if let Some(v) = local.cpu_quota_pct {
                base.local.cpu_quota_pct = v;
            }
            if let Some(v) = local.memory_max {
                base.local.memory_max = v;
            }
            if let Some(v) = local.cap_passthrough {
                base.local.cap_passthrough = v;
            }
        }

        // Merge tool configs. A tool section without an `intercept` key
        // inherits the lower layer's intercept list; naming a tool that no
        // lower layer mentioned opts it in with the default `["test"]`. An
        // explicit `intercept = []` narrows the tool to never intercept.
        for (tool_name, raw_tool) in raw.tool {
            let intercept = match raw_tool.intercept {
                Some(v) => v,
                None => base
                    .tools
                    .get(&tool_name)
                    .map(|t| t.intercept.clone())
                    .unwrap_or_else(|| vec!["test".to_string()]),
            };
            let real_binary = match raw_tool.real_binary {
                Some(v) => Some(PathBuf::from(v)),
                None => base
                    .tools
                    .get(&tool_name)
                    .and_then(|t| t.real_binary.clone()),
            };
            base.tools.insert(
                tool_name,
                ToolConfig {
                    intercept,
                    real_binary,
                },
            );
        }

        // Merge remote config with trust boundary.
        if let Some(remote) = raw.remote {
            // Backend: only an explicit key overrides. `command` from the
            // repo layer rejects the whole layer (fail-closed to LKG) — a
            // repo-chosen executable template is arbitrary code execution.
            if let Some(backend) = remote.backend {
                base.remote.backend = match backend.as_str() {
                    "none" => Backend::None,
                    "argo" => Backend::Argo,
                    "command" => {
                        if repo_layer {
                            return Err("repo config cannot set backend to 'command' \
                                        (trust boundary S-2)"
                                .to_string());
                        }
                        Backend::Command
                    }
                    _ => {
                        warnings.push(format!("unknown backend '{backend}', using 'none'"));
                        Backend::None
                    }
                };
            }

            // Trust boundary (S-2): the repo layer cannot set ci_remote or
            // push_mode. The key is ignored with a warning rather than
            // rejecting the layer — a cloned repo must not redirect pushes,
            // but a stray restricted key should not discard the repo's own
            // intercept narrowing.
            match remote.ci_remote {
                Some(_) if repo_layer => warnings.push(
                    "repo config cannot set 'ci_remote' (trust boundary S-2), ignoring".to_string(),
                ),
                Some(v) => base.remote.ci_remote = v,
                None => {}
            }
            match remote.push_mode {
                Some(_) if repo_layer => warnings.push(
                    "repo config cannot set 'push_mode' (trust boundary S-2), ignoring".to_string(),
                ),
                Some(v) => {
                    base.remote.push_mode = match v.as_str() {
                        "ref" => PushMode::Ref,
                        "branch" => PushMode::Branch,
                        _ => {
                            warnings.push(format!("unknown push_mode '{v}', using 'ref'"));
                            PushMode::Ref
                        }
                    }
                }
                None => {}
            }

            if let Some(v) = remote.deadline_minutes {
                base.remote.deadline_minutes = v;
            }

            // Argo config: key-granular onto whatever a lower layer built.
            if let Some(argo) = remote.argo {
                let mut merged = base.remote.argo.take().unwrap_or_default();
                if let Some(v) = argo.kubectl_path {
                    merged.kubectl_path = v;
                }
                if let Some(v) = argo.kubeconfig {
                    merged.kubeconfig = Self::expand_home(&v);
                }
                if let Some(v) = argo.namespace {
                    merged.namespace = v;
                }
                if let Some(v) = argo.template {
                    merged.template = v;
                }
                if let Some(v) = argo.generate_name {
                    merged.generate_name = v;
                }
                if let Some(v) = argo.builder_image {
                    merged.builder_image = Some(v);
                }
                if let Some(v) = argo.base_url {
                    merged.base_url = Some(v);
                }
                base.remote.argo = Some(merged);
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

    /// Try to get the real binary override for `tool`, returning a Result.
    ///
    /// This is a convenience method for doctor to get the override or None
    /// without needing to handle Option<&PathBuf> lifetime issues.
    pub fn try_get_real_binary(&self) -> Result<Option<std::path::PathBuf>, String> {
        Ok(self.real_binary("cargo").cloned())
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
        Self::repo_config_path_in(Path::new("."))
    }

    /// [`repo_config_path`], searched upward from an explicit start directory
    /// instead of the process cwd, so tests can exercise the walk against
    /// fixture trees without mutating the process-wide current directory.
    fn repo_config_path_in(start: &Path) -> Option<PathBuf> {
        // Find repo root by looking for .git directory.
        let mut path = start.canonicalize().ok()?;

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

    fn lkg_path_in(state_dir: &Path) -> PathBuf {
        state_dir.join("last-known-good.toml")
    }

    fn lkg_meta_path_in(state_dir: &Path) -> PathBuf {
        state_dir.join("last-known-good.meta.json")
    }

    fn broken_marker_path_in(state_dir: &Path) -> PathBuf {
        state_dir.join("broken-config.marker")
    }

    /// Persist the config as the last-known-good snapshot under `state_dir`.
    fn persist_lkg_in(state_dir: &Path, config: &GantryConfig) -> Result<(), String> {
        fs::create_dir_all(state_dir).map_err(|e| format!("failed to create state dir: {}", e))?;

        // Serialize config to TOML.
        let toml_content = toml::to_string_pretty(&Self::to_raw(config))
            .map_err(|e| format!("failed to serialize config: {}", e))?;

        // Write the snapshot.
        let lkg_path = Self::lkg_path_in(state_dir);
        fs::write(&lkg_path, toml_content)
            .map_err(|e| format!("failed to write LKG snapshot: {}", e))?;

        // Write metadata.
        let meta = LkgMetadata {
            schema_version: 1,
            created_at: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            // The snapshot is the merged product of all layers, not the
            // product of any single file layer.
            source: ConfigLayer::Defaults,
        };
        let meta_json = serde_json::to_string_pretty(&meta)
            .map_err(|e| format!("failed to serialize metadata: {}", e))?;
        fs::write(Self::lkg_meta_path_in(state_dir), meta_json)
            .map_err(|e| format!("failed to write LKG metadata: {}", e))?;

        Ok(())
    }

    /// Load the last-known-good snapshot from `state_dir`, maintaining the
    /// escalating broken-config banner state as a side effect.
    fn load_lkg_in(
        state_dir: &Path,
        warnings: &mut Vec<String>,
        broken_banner: &mut Option<BrokenBanner>,
    ) -> Result<GantryConfig, String> {
        let lkg_path = Self::lkg_path_in(state_dir);
        let meta_path = Self::lkg_meta_path_in(state_dir);
        let marker_path = Self::broken_marker_path_in(state_dir);

        // The state dir is normally created by persist_lkg_in on an earlier
        // clean load — but a config that has never loaded cleanly reaches
        // here with no state dir, and the marker must land anyway or the
        // banner can never escalate across runs.
        let _ = fs::create_dir_all(state_dir);

        // Escalating banner state: a fresh corruption starts the count at 1
        // and stamps "broken since"; every further degraded run increments
        // the count but keeps the original timestamp. A marker that cannot
        // be read as {count, since} is treated as a fresh detection and
        // overwritten — a broken marker must never suppress the banner,
        // because silence is the one failure mode Q-7 forbids.
        let banner = match fs::read_to_string(&marker_path)
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|marker| {
                let count = marker["count"].as_u64()?;
                let since = marker["since"].as_u64()?;
                Some((count, since))
            }) {
            Some((count, since_ts)) => {
                let banner = BrokenBanner {
                    count: count + 1,
                    since: SystemTime::UNIX_EPOCH + Duration::from_secs(since_ts),
                };

                // Update marker with incremented count.
                let updated = serde_json::json!({
                    "count": banner.count,
                    "since": since_ts,
                });
                let _ = fs::write(&marker_path, updated.to_string());
                banner
            }
            None => {
                // First detection (or unreadable marker) - create marker.
                let now = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let marker = serde_json::json!({ "count": 1, "since": now });
                let _ = fs::write(&marker_path, marker.to_string());
                BrokenBanner {
                    count: 1,
                    since: SystemTime::UNIX_EPOCH + Duration::from_secs(now),
                }
            }
        };
        *broken_banner = Some(banner);

        // Read and validate metadata.
        let meta_content = fs::read_to_string(&meta_path)
            .map_err(|e| format!("failed to read LKG metadata: {}", e))?;
        let _meta: LkgMetadata = serde_json::from_str(&meta_content)
            .map_err(|e| format!("LKG metadata corrupted: {}", e))?;

        // Read and parse the snapshot.
        let _snapshot_content = fs::read_to_string(&lkg_path)
            .map_err(|e| format!("failed to read LKG snapshot: {}", e))?;

        // Parse using merge_layer with no trust boundary: the snapshot was
        // written by gantry itself, not supplied by the repo layer.
        let mut config = Self::tier_0_defaults();
        Self::merge_layer(&mut config, &lkg_path, ConfigLayer::Defaults, warnings)?;

        Ok(config)
    }

    /// Remove the broken-config marker after a clean load.
    ///
    /// The banner runs "until fixed" (plan Q-7): a config that parses must
    /// stop the escalation, and the next corruption must start a fresh count
    /// and "since" rather than inheriting the previous incident's. Best
    /// effort — failing to remove the marker must never fail the load.
    fn clear_broken_marker_in(state_dir: &Path) {
        let _ = fs::remove_file(Self::broken_marker_path_in(state_dir));
    }

    /// Convert Config to RawConfig for serialization.
    fn to_raw(config: &GantryConfig) -> RawConfig {
        RawConfig {
            local: Some(RawLocal {
                cpu_quota_pct: Some(config.local.cpu_quota_pct),
                memory_max: Some(config.local.memory_max.clone()),
                cap_passthrough: Some(config.local.cap_passthrough),
                unknown: HashMap::new(),
            }),
            tool: config
                .tools
                .iter()
                .map(|(name, tool)| {
                    (
                        name.clone(),
                        RawTool {
                            intercept: Some(tool.intercept.clone()),
                            real_binary: tool
                                .real_binary
                                .as_ref()
                                .map(|p| p.to_string_lossy().to_string()),
                            unknown: HashMap::new(),
                        },
                    )
                })
                .collect(),
            remote: Some(RawRemote {
                backend: Some(match &config.remote.backend {
                    Backend::None => "none".to_string(),
                    Backend::Argo => "argo".to_string(),
                    Backend::Command => "command".to_string(),
                }),
                ci_remote: Some(config.remote.ci_remote.clone()),
                push_mode: Some(match &config.remote.push_mode {
                    PushMode::Ref => "ref".to_string(),
                    PushMode::Branch => "branch".to_string(),
                }),
                deadline_minutes: Some(config.remote.deadline_minutes),
                argo: config.remote.argo.as_ref().map(|a| RawArgo {
                    kubectl_path: Some(a.kubectl_path.clone()),
                    kubeconfig: Some(a.kubeconfig.to_string_lossy().to_string()),
                    namespace: Some(a.namespace.clone()),
                    template: Some(a.template.clone()),
                    generate_name: Some(a.generate_name.clone()),
                    builder_image: a.builder_image.clone(),
                    base_url: a.base_url.clone(),
                    unknown: HashMap::new(),
                }),
                command: config.remote.command.as_ref().map(|c| RawCommand {
                    submit: c.submit.clone(),
                    logs: c.logs.clone(),
                    wait: c.wait.clone(),
                    unknown: HashMap::new(),
                }),
                unknown: HashMap::new(),
            }),
            unknown: HashMap::new(),
        }
    }
}

// ============================================================================
// Unknown-key warnings
// ============================================================================

/// Emit one warning per unknown key captured by a raw struct's flatten map.
///
/// Forward compatibility: keys from a newer config schema (or plain typos)
/// never fail the load — they are reported and ignored. Keys are sorted so
/// warning output is deterministic for a given file.
fn warn_unknown_keys(
    unknown: &HashMap<String, toml::Value>,
    section: &str,
    layer: ConfigLayer,
    warnings: &mut Vec<String>,
) {
    let mut keys: Vec<&String> = unknown.keys().collect();
    keys.sort();
    for key in keys {
        if section.is_empty() {
            warnings.push(format!("unknown key '{key}' in {} config, ignoring", layer));
        } else {
            warnings.push(format!(
                "unknown key '{section}.{key}' in {} config, ignoring",
                layer
            ));
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

    /// Wrapper so ConfigLayer can be exercised through real toml/JSON
    /// documents rather than just the serializer's raw string output.
    #[derive(Deserialize, Serialize)]
    struct ConfigLayerWrapper {
        layer: ConfigLayer,
    }

    #[test]
    fn config_layer_serde_round_trips_all_variants() {
        let layers = [
            (ConfigLayer::System, "system"),
            (ConfigLayer::User, "user"),
            (ConfigLayer::Repo, "repo"),
            (ConfigLayer::Defaults, "defaults"),
        ];

        for (layer, name) in layers {
            // JSON string form, both directions.
            assert_eq!(
                serde_json::to_string(&layer).unwrap(),
                format!("\"{name}\"")
            );
            assert_eq!(
                serde_json::from_str::<ConfigLayer>(&format!("\"{name}\"")).unwrap(),
                layer
            );

            // TOML string form, both directions.
            let wrapper = ConfigLayerWrapper { layer };
            assert_eq!(
                toml::to_string(&wrapper).unwrap(),
                format!("layer = \"{name}\"\n")
            );
            let parsed: ConfigLayerWrapper =
                toml::from_str(&format!("layer = \"{name}\"")).unwrap();
            assert_eq!(parsed.layer, layer);
        }

        // Distinct variants compare distinctly (PartialEq/Eq semantics).
        assert_ne!(ConfigLayer::System, ConfigLayer::Repo);
        assert_ne!(ConfigLayer::User, ConfigLayer::Defaults);
    }

    #[test]
    fn config_layer_display_matches_serde_form() {
        assert_eq!(ConfigLayer::System.to_string(), "system");
        assert_eq!(ConfigLayer::User.to_string(), "user");
        assert_eq!(ConfigLayer::Repo.to_string(), "repo");
        assert_eq!(ConfigLayer::Defaults.to_string(), "defaults");
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

        // Explicit start directory — the process cwd is never touched.
        let result = Config::repo_config_path_in(&repo_root);
        assert_eq!(result, Some(repo_root.join(".gantry.toml")));
    }

    #[test]
    fn repo_config_path_walks_up_from_subdirectory() {
        let temp = TempDir::new().unwrap();
        let repo_root = temp.path().join("test_repo");
        let nested = repo_root.join("src").join("deep");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir(repo_root.join(".git")).unwrap();

        let result = Config::repo_config_path_in(&nested);
        assert_eq!(result, Some(repo_root.join(".gantry.toml")));
    }

    #[test]
    fn repo_config_path_returns_none_outside_git() {
        let temp = TempDir::new().unwrap();

        let result = Config::repo_config_path_in(temp.path());
        assert!(result.is_none());
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
        Config::merge_layer(&mut cfg, &config, ConfigLayer::User, &mut warnings).unwrap();

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
        Config::merge_layer(&mut cfg, &config, ConfigLayer::User, &mut warnings).unwrap();

        assert!(cfg.intercepts("cargo", "test"));
        assert!(cfg.intercepts("cargo", "check"));
        assert!(!cfg.intercepts("cargo", "build"));
    }

    /// A zero-byte config file merged at any layer is a no-op: Ok, no
    /// warnings, and the Tier-0 baseline untouched (bf-2fef acceptance:
    /// "can parse empty config files").
    #[test]
    fn merge_layer_empty_file_is_noop_for_every_layer() {
        for layer in [ConfigLayer::System, ConfigLayer::User, ConfigLayer::Repo] {
            let mut cfg = Config::tier_0_defaults();
            let temp = TempDir::new().unwrap();
            let config = write_test_config(temp.path(), "");
            assert_eq!(
                fs::metadata(&config).unwrap().len(),
                0,
                "{layer}: fixture not empty"
            );

            let mut warnings = Vec::new();
            Config::merge_layer(&mut cfg, &config, layer, &mut warnings).unwrap();

            assert!(warnings.is_empty(), "{layer}: warnings: {warnings:?}");
            assert_eq!(cfg, Config::tier_0_defaults(), "{layer}: baseline drifted");
        }
    }

    /// A file containing only comments and whitespace parses identically to
    /// a zero-byte one: Ok, no warnings, Tier-0 baseline untouched.
    #[test]
    fn merge_layer_comments_and_whitespace_only_is_noop_for_every_layer() {
        let content = "\
# gantry config — commentary only, no keys.
# Another comment line.

\t
        # an indented comment
";

        for layer in [ConfigLayer::System, ConfigLayer::User, ConfigLayer::Repo] {
            let mut cfg = Config::tier_0_defaults();
            let temp = TempDir::new().unwrap();
            let config = write_test_config(temp.path(), content);

            let mut warnings = Vec::new();
            Config::merge_layer(&mut cfg, &config, layer, &mut warnings).unwrap();

            assert!(warnings.is_empty(), "{layer}: warnings: {warnings:?}");
            assert_eq!(cfg, Config::tier_0_defaults(), "{layer}: baseline drifted");
        }
    }

    /// An empty `[tool.<name>]` section — table header present, no keys —
    /// parses to the cargo-default intercept behavior: `["test"]` with no
    /// real_binary override, and no warnings.
    #[test]
    fn merge_layer_empty_tool_section_gets_cargo_default_intercept() {
        let mut cfg = Config::tier_0_defaults();
        let temp = TempDir::new().unwrap();
        let config = write_test_config(
            temp.path(),
            r#"
            [tool.cargo]

            [tool.nextest]
            "#,
        );

        let mut warnings = Vec::new();
        Config::merge_layer(&mut cfg, &config, ConfigLayer::Repo, &mut warnings).unwrap();

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert!(cfg.intercepts("cargo", "test"));
        assert!(cfg.intercepts("nextest", "test"));
        assert!(!cfg.intercepts("cargo", "build"));

        let expected = ToolConfig {
            intercept: vec!["test".to_string()],
            real_binary: None,
        };
        assert_eq!(cfg.tools.get("cargo"), Some(&expected));
        assert_eq!(cfg.tools.get("nextest"), Some(&expected));
    }

    /// A repo whose `.gantry.toml` is empty resolves through the upward walk
    /// and would load without error: merging the resolved path at the repo
    /// layer is Ok, warning-free, and leaves the Tier-0 baseline.
    #[test]
    fn repo_config_path_resolves_empty_gantry_toml_and_loads_clean() {
        let temp = TempDir::new().unwrap();
        let repo_root = temp.path().join("empty_repo");
        fs::create_dir_all(&repo_root).unwrap();
        fs::create_dir(repo_root.join(".git")).unwrap();
        let gantry_toml = repo_root.join(".gantry.toml");
        fs::write(&gantry_toml, "").unwrap();

        let resolved = Config::repo_config_path_in(&repo_root).expect("repo config path");
        assert_eq!(resolved, gantry_toml);
        assert_eq!(fs::metadata(&resolved).unwrap().len(), 0);

        let mut cfg = Config::tier_0_defaults();
        let mut warnings = Vec::new();
        Config::merge_layer(&mut cfg, &resolved, ConfigLayer::Repo, &mut warnings).unwrap();

        assert!(warnings.is_empty(), "warnings: {warnings:?}");
        assert_eq!(cfg, Config::tier_0_defaults());
    }

    /// The [remote.argo] block deserializes with its documented defaults:
    /// kubectl resolved via PATH, builder-image omitted so the
    /// WorkflowTemplate default applies (backend/argo.rs drops the parameter
    /// from the manifest when it is None).
    #[test]
    fn merge_layer_argo_defaults_kubectl_path_and_builder_image() {
        let mut cfg = Config::tier_0_defaults();
        let temp = TempDir::new().unwrap();
        let config = write_test_config(
            temp.path(),
            r#"
            [remote]
            backend = "argo"

            [remote.argo]
            "#,
        );

        let mut warnings = Vec::new();
        Config::merge_layer(&mut cfg, &config, ConfigLayer::User, &mut warnings).unwrap();

        let argo = cfg.remote.argo.as_ref().expect("argo config present");
        assert_eq!(argo.kubectl_path, "kubectl");
        assert_eq!(argo.builder_image, None);
        assert_eq!(argo.template, "gantry-verify");
        assert_eq!(argo.generate_name, "gantry-");
        assert_eq!(argo.base_url, None);
    }

    /// Explicit kubectl_path and builder_image values flow through the merge
    /// untouched — these are user-layer (trusted) knobs per S-2.
    #[test]
    fn merge_layer_argo_explicit_kubectl_path_and_builder_image() {
        let mut cfg = Config::tier_0_defaults();
        let temp = TempDir::new().unwrap();
        let config = write_test_config(
            temp.path(),
            r#"
            [remote]
            backend = "argo"

            [remote.argo]
            kubectl_path = "/usr/local/bin/kubectl"
            builder_image = "ronaldraygun/gantry-builder:1.83"
            base_url = "https://argo-ci.example.com"
            "#,
        );

        let mut warnings = Vec::new();
        Config::merge_layer(&mut cfg, &config, ConfigLayer::User, &mut warnings).unwrap();

        let argo = cfg.remote.argo.as_ref().expect("argo config present");
        assert_eq!(argo.kubectl_path, "/usr/local/bin/kubectl");
        assert_eq!(
            argo.builder_image,
            Some("ronaldraygun/gantry-builder:1.83".to_string())
        );
        assert_eq!(
            argo.base_url,
            Some("https://argo-ci.example.com".to_string())
        );
    }

    /// The last-known-good snapshot path — to_raw, TOML serialize, reparse at
    /// the Defaults layer — preserves the full [remote.argo] block. A field
    /// added to RawArgo but missed in to_raw would otherwise drop silently
    /// from every snapshot persist_lkg writes.
    #[test]
    fn argo_config_survives_lkg_snapshot_round_trip() {
        let mut cfg = Config::tier_0_defaults();
        let temp = TempDir::new().unwrap();
        let config = write_test_config(
            temp.path(),
            r#"
            [remote]
            backend = "argo"

            [remote.argo]
            kubectl_path = "/usr/local/bin/kubectl"
            kubeconfig = "/etc/gantry-test/kubeconfig"
            namespace = "argo-workflows"
            template = "gantry-verify"
            generate_name = "gantry-"
            builder_image = "ronaldraygun/gantry-builder:1.83"
            base_url = "https://argo-ci.example.com"
            "#,
        );
        let mut warnings = Vec::new();
        Config::merge_layer(&mut cfg, &config, ConfigLayer::User, &mut warnings).unwrap();
        assert!(warnings.is_empty(), "warnings: {warnings:?}");

        // Exactly what persist_lkg and load_lkg do around the snapshot file.
        let serialized = toml::to_string_pretty(&Config::to_raw(&cfg)).expect("serialize snapshot");
        let snapshot_path = temp.path().join("last-known-good.toml");
        fs::write(&snapshot_path, &serialized).unwrap();

        let mut restored = Config::tier_0_defaults();
        let mut warnings = Vec::new();
        Config::merge_layer(
            &mut restored,
            &snapshot_path,
            ConfigLayer::Defaults,
            &mut warnings,
        )
        .unwrap();
        assert!(warnings.is_empty(), "warnings: {warnings:?}");

        let argo = restored.remote.argo.as_ref().expect("argo config restored");
        assert_eq!(argo.kubectl_path, "/usr/local/bin/kubectl");
        assert_eq!(
            argo.builder_image,
            Some("ronaldraygun/gantry-builder:1.83".to_string())
        );
        assert_eq!(
            restored.remote.argo.as_ref(),
            cfg.remote.argo.as_ref(),
            "argo block drifted across the LKG round trip"
        );
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
        Config::merge_layer(&mut cfg, &config, ConfigLayer::Repo, &mut warnings).unwrap();

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
        Config::merge_layer(&mut cfg, &config, ConfigLayer::Repo, &mut warnings).unwrap();

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
        let result = Config::merge_layer(&mut cfg, &config, ConfigLayer::Repo, &mut warnings);

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
        Config::merge_layer(&mut cfg, &config, ConfigLayer::User, &mut warnings).unwrap();

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
        Config::merge_layer(&mut cfg, &config, ConfigLayer::User, &mut warnings).unwrap();

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

    // ========================================================================
    // Three-layer merging end-to-end (bf-37ng)
    // ========================================================================

    /// Helper: write a layer file and return its path (None = layer absent).
    fn layer_file(dir: &Path, name: &str, content: &str) -> Option<PathBuf> {
        if content.is_empty() {
            return None;
        }
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        Some(path)
    }

    #[test]
    fn layering_composes_across_all_three_layers() {
        let temp = TempDir::new().unwrap();
        let system = layer_file(
            temp.path(),
            "system.toml",
            r#"
            [local]
            cpu_quota_pct = 100
            "#,
        );
        let user = layer_file(
            temp.path(),
            "user.toml",
            r#"
            [local]
            memory_max = "12G"

            [tool.cargo]
            intercept = ["test", "check"]
            "#,
        );
        let repo = layer_file(
            temp.path(),
            "repo.toml",
            r#"
            [tool.nextest]

            [remote]
            deadline_minutes = 55
            "#,
        );

        let result =
            Config::load_layers(system.as_deref(), user.as_deref(), repo.as_deref()).unwrap();

        // Keys set by different layers all land: layers compose key by key,
        // they do not replace each other section by section.
        assert_eq!(result.config.local.cpu_quota_pct, 100, "system layer");
        assert_eq!(result.config.local.memory_max, "12G", "user layer");
        assert_eq!(result.config.remote.deadline_minutes, 55, "repo layer");
        assert!(result.config.intercepts("cargo", "test"));
        assert!(result.config.intercepts("cargo", "check"));
        assert!(result.config.intercepts("nextest", "test"));
        assert!(
            result.warnings.is_empty(),
            "warnings: {:?}",
            result.warnings
        );
    }

    #[test]
    fn layering_same_key_repo_beats_user_beats_system() {
        let temp = TempDir::new().unwrap();
        let system = layer_file(
            temp.path(),
            "system.toml",
            r#"
            [local]
            cpu_quota_pct = 100

            [remote]
            ci_remote = "upstream"
            "#,
        );
        let user = layer_file(
            temp.path(),
            "user.toml",
            r#"
            [local]
            cpu_quota_pct = 150

            [remote]
            ci_remote = "mirror"
            "#,
        );
        let repo = layer_file(
            temp.path(),
            "repo.toml",
            r#"
            [local]
            cpu_quota_pct = 175
            "#,
        );

        let result =
            Config::load_layers(system.as_deref(), user.as_deref(), repo.as_deref()).unwrap();

        assert_eq!(result.config.local.cpu_quota_pct, 175);
        assert_eq!(result.config.remote.ci_remote, "mirror");
    }

    #[test]
    fn layering_absent_middle_layer_is_skipped() {
        let temp = TempDir::new().unwrap();
        let system = layer_file(
            temp.path(),
            "system.toml",
            r#"
            [local]
            cpu_quota_pct = 100
            "#,
        );
        let repo = layer_file(
            temp.path(),
            "repo.toml",
            r#"
            [remote]
            deadline_minutes = 50
            "#,
        );

        // User layer absent (None): system and repo still compose.
        let result = Config::load_layers(system.as_deref(), None, repo.as_deref()).unwrap();
        assert_eq!(result.config.local.cpu_quota_pct, 100);
        assert_eq!(result.config.remote.deadline_minutes, 50);
        assert!(
            result.warnings.is_empty(),
            "warnings: {:?}",
            result.warnings
        );
    }

    #[test]
    fn layering_no_layers_yields_tier0_with_no_warnings() {
        // All layers absent, and paths that simply do not exist.
        let missing = TempDir::new().unwrap().path().join("nope.toml");
        for (system, user, repo) in [
            (None, None, None),
            (
                Some(missing.as_path()),
                Some(missing.as_path()),
                Some(missing.as_path()),
            ),
        ] {
            let result = Config::load_layers(system, user, repo).unwrap();
            assert_eq!(result.config, Config::tier_0_defaults());
            assert!(
                result.warnings.is_empty(),
                "warnings: {:?}",
                result.warnings
            );
            assert!(result.broken_banner.is_none());
        }
    }

    /// The point of key-granular merging: a repo `[remote]` that touches one
    /// key must not reset the backend a user layer chose (the old
    /// section-granular merge clobbered it back to Tier-0's "none").
    #[test]
    fn layering_remote_sections_compose_key_by_key() {
        let temp = TempDir::new().unwrap();
        let system = layer_file(temp.path(), "system.toml", "");
        let user = layer_file(
            temp.path(),
            "user.toml",
            r#"
            [remote]
            backend = "argo"
            deadline_minutes = 10
            "#,
        );
        let repo = layer_file(
            temp.path(),
            "repo.toml",
            r#"
            [remote]
            deadline_minutes = 50
            "#,
        );

        let result =
            Config::load_layers(system.as_deref(), user.as_deref(), repo.as_deref()).unwrap();

        assert_eq!(result.config.remote.backend, Backend::Argo);
        assert_eq!(result.config.remote.deadline_minutes, 50);
    }

    #[test]
    fn layering_argo_block_composes_across_layers() {
        let temp = TempDir::new().unwrap();
        let user = layer_file(
            temp.path(),
            "user.toml",
            r#"
            [remote]
            backend = "argo"

            [remote.argo]
            kubectl_path = "/usr/local/bin/kubectl"
            template = "user-template"
            "#,
        );
        let repo = layer_file(
            temp.path(),
            "repo.toml",
            r#"
            [remote]

            [remote.argo]
            template = "repo-template"
            "#,
        );

        let result = Config::load_layers(None, user.as_deref(), repo.as_deref()).unwrap();

        let argo = result.config.remote.argo.as_ref().expect("argo config");
        assert_eq!(argo.kubectl_path, "/usr/local/bin/kubectl", "user key kept");
        assert_eq!(argo.template, "repo-template", "repo key wins");
        assert_eq!(argo.namespace, "argo-workflows", "default fills the rest");
    }

    #[test]
    fn layering_tool_config_composes_across_layers() {
        let temp = TempDir::new().unwrap();
        let user = layer_file(
            temp.path(),
            "user.toml",
            r#"
            [tool.cargo]
            intercept = ["test", "check"]
            real_binary = "/custom/cargo"
            "#,
        );
        let repo = layer_file(
            temp.path(),
            "repo.toml",
            r#"
            [tool.cargo]
            intercept = ["test", "clippy"]
            "#,
        );

        let result = Config::load_layers(None, user.as_deref(), repo.as_deref()).unwrap();

        let cargo = result.config.tools.get("cargo").expect("cargo tool");
        assert_eq!(cargo.intercept, vec!["test", "clippy"], "repo narrows");
        // real_binary not repeated by the repo layer — the user's choice holds.
        assert_eq!(cargo.real_binary, Some(PathBuf::from("/custom/cargo")));
    }

    /// Repo-level narrowing all the way down: an explicit empty intercept
    /// list disables interception for that tool.
    #[test]
    fn layering_explicit_empty_intercept_narrows_tool_to_never() {
        let temp = TempDir::new().unwrap();
        let repo = layer_file(
            temp.path(),
            "repo.toml",
            r#"
            [tool.cargo]
            intercept = []
            "#,
        );

        let result = Config::load_layers(None, None, repo.as_deref()).unwrap();

        let cargo = result.config.tools.get("cargo").expect("cargo tool");
        assert!(cargo.intercept.is_empty());
        assert!(!result.config.intercepts("cargo", "test"));
    }

    /// Full resolution path, no injected shortcuts: the cwd-style upward walk
    /// finds the repo root's .gantry.toml from a nested directory, and the
    /// found file merges under the system layer with the trust boundary.
    #[test]
    fn repo_walk_feeds_repo_layer_end_to_end() {
        let temp = TempDir::new().unwrap();
        let repo_root = temp.path().join("walker");
        let deep = repo_root.join("src").join("deep");
        fs::create_dir_all(&deep).unwrap();
        fs::create_dir(repo_root.join(".git")).unwrap();
        fs::write(
            repo_root.join(".gantry.toml"),
            r#"
            [tool.cargo]
            intercept = ["test", "clippy"]
            "#,
        )
        .unwrap();

        let resolved = Config::repo_config_path_in(&deep).expect("walk finds repo config");

        let result = Config::load_layers(None, None, Some(resolved.as_path())).unwrap();
        assert!(result.config.intercepts("cargo", "clippy"));
        assert!(result.config.intercepts("cargo", "test"));
        assert!(!result.config.intercepts("cargo", "build"));
    }

    // ========================================================================
    // Trust boundary (S-2) with warnings
    // ========================================================================

    /// Restricted keys in `.gantry.toml` are ignored with a loud warning,
    /// but the rest of the repo layer still applies — a stray `ci_remote`
    /// must not discard the repo's own intercept narrowing.
    #[test]
    fn trust_boundary_repo_restricted_keys_ignored_with_warning() {
        let temp = TempDir::new().unwrap();
        let repo = layer_file(
            temp.path(),
            "repo.toml",
            r#"
            [remote]
            ci_remote = "attacker-controlled"
            push_mode = "branch"

            [tool.cargo]
            intercept = ["test", "clippy"]
            "#,
        );

        let result = Config::load_layers(None, None, repo.as_deref()).unwrap();

        assert_eq!(
            result.config.remote.ci_remote, "origin",
            "ci_remote blocked"
        );
        assert_eq!(
            result.config.remote.push_mode,
            PushMode::Ref,
            "push_mode blocked"
        );
        assert!(result.config.intercepts("cargo", "clippy"), "rest applies");

        let ci = result
            .warnings
            .iter()
            .any(|w| w.contains("ci_remote") && w.contains("trust boundary"));
        let push = result
            .warnings
            .iter()
            .any(|w| w.contains("push_mode") && w.contains("trust boundary"));
        assert!(ci, "ci_remote warning missing: {:?}", result.warnings);
        assert!(push, "push_mode warning missing: {:?}", result.warnings);
    }

    /// The boundary is one-way: trusted layers may still set both keys.
    #[test]
    fn trust_boundary_trusted_layers_can_set_ci_remote_and_push_mode() {
        let temp = TempDir::new().unwrap();
        let user = layer_file(
            temp.path(),
            "user.toml",
            r#"
            [remote]
            ci_remote = "upstream"
            push_mode = "branch"
            "#,
        );

        let result = Config::load_layers(None, user.as_deref(), None).unwrap();

        assert_eq!(result.config.remote.ci_remote, "upstream");
        assert_eq!(result.config.remote.push_mode, PushMode::Branch);
        assert!(
            result.warnings.is_empty(),
            "warnings: {:?}",
            result.warnings
        );
    }

    /// Repo layer restricting the backend to Tier-0 (narrowing) is allowed.
    #[test]
    fn trust_boundary_repo_can_narrow_backend_to_none() {
        let temp = TempDir::new().unwrap();
        let user = layer_file(
            temp.path(),
            "user.toml",
            r#"
            [remote]
            backend = "argo"
            "#,
        );
        let repo = layer_file(
            temp.path(),
            "repo.toml",
            r#"
            [remote]
            backend = "none"
            "#,
        );

        let result = Config::load_layers(None, user.as_deref(), repo.as_deref()).unwrap();

        assert_eq!(result.config.remote.backend, Backend::None);
        assert!(
            result.warnings.is_empty(),
            "warnings: {:?}",
            result.warnings
        );
    }

    // ========================================================================
    // Unknown keys — warn, never error
    // ========================================================================

    #[test]
    fn unknown_top_level_keys_warn_but_config_loads() {
        let temp = TempDir::new().unwrap();
        let user = layer_file(
            temp.path(),
            "user.toml",
            r#"
            banana = true

            [other_bogus]
            key = 1

            [local]
            cpu_quota_pct = 120
            "#,
        );

        let result = Config::load_layers(None, user.as_deref(), None).unwrap();

        // Recognized keys still apply.
        assert_eq!(result.config.local.cpu_quota_pct, 120);

        let banana = result
            .warnings
            .iter()
            .any(|w| w.contains("unknown key 'banana'") && w.contains("user"));
        let bogus = result
            .warnings
            .iter()
            .any(|w| w.contains("unknown key 'other_bogus'"));
        assert!(banana, "banana warning missing: {:?}", result.warnings);
        assert!(bogus, "other_bogus warning missing: {:?}", result.warnings);
    }

    #[test]
    fn unknown_nested_keys_warn_with_section_path() {
        let temp = TempDir::new().unwrap();
        let user = layer_file(
            temp.path(),
            "user.toml",
            r#"
            [local]
            cpu_quota_pct = 120
            memry_max = "8G"

            [tool.cargo]
            intercept = ["test"]
            interceptt = ["chek"]

            [remote]
            backend = "argo"
            deadline_minuts = 5

            [remote.argo]
            namespaces = "wrong"
            "#,
        );

        let result = Config::load_layers(None, user.as_deref(), None).unwrap();

        assert_eq!(result.config.local.cpu_quota_pct, 120, "known keys apply");
        assert_eq!(result.config.remote.backend, Backend::Argo);

        for expected in [
            "local.memry_max",
            "tool.cargo.interceptt",
            "remote.deadline_minuts",
            "remote.argo.namespaces",
        ] {
            let found = result
                .warnings
                .iter()
                .any(|w| w.contains(&format!("unknown key '{expected}'")));
            assert!(
                found,
                "warning for '{expected}' missing: {:?}",
                result.warnings
            );
        }
    }

    /// Unknown keys must never fail the load regardless of value shape —
    /// including TOML datetimes, which is why the flatten maps hold
    /// `toml::Value` rather than `serde_json::Value`.
    #[test]
    fn unknown_keys_with_exotic_values_warn_but_never_error() {
        let temp = TempDir::new().unwrap();
        let user = layer_file(
            temp.path(),
            "user.toml",
            r#"
            created = 1979-05-27T07:32:00Z
            tags = ["a", "b"]
            nested = { x = 1 }

            [local]
            cap_passthrough = false
            "#,
        );

        let result = Config::load_layers(None, user.as_deref(), None).unwrap();

        assert!(!result.config.local.cap_passthrough, "known keys apply");
        for expected in ["created", "tags", "nested"] {
            let found = result
                .warnings
                .iter()
                .any(|w| w.contains(&format!("unknown key '{expected}'")));
            assert!(
                found,
                "warning for '{expected}' missing: {:?}",
                result.warnings
            );
        }
    }

    /// Warnings come out in a stable order (sorted keys) so logs are
    /// diffable across runs.
    #[test]
    fn unknown_key_warnings_are_deterministic() {
        let temp = TempDir::new().unwrap();
        let user = layer_file(
            temp.path(),
            "user.toml",
            "zebra = 1\napple = 2\nmango = 3\n",
        );

        let result = Config::load_layers(None, user.as_deref(), None).unwrap();

        let keys: Vec<&str> = result
            .warnings
            .iter()
            .filter_map(|w| {
                w.split("unknown key '")
                    .nth(1)
                    .map(|rest| rest.split('\'').next().unwrap())
            })
            .collect();
        assert_eq!(keys, vec!["apple", "mango", "zebra"]);
    }

    // ========================================================================
    // Last-known-good snapshot + escalating banner (bf-10pd, Q-7)
    // ========================================================================

    /// Config content that always fails to parse — the corruption fixture.
    /// Unknown keys would only warn, so a broken config must be a structural
    /// one.
    const CORRUPT_TOML: &str = ":: definitely not toml ::";

    /// A healthy user config loads clean and leaves an LKG snapshot behind:
    /// no banner, and both snapshot files in the state dir.
    #[test]
    fn lkg_clean_load_refreshes_snapshot_without_banner() {
        let temp = TempDir::new().unwrap();
        let state = temp.path().join("state");
        let user = temp.path().join("config.toml");
        fs::write(
            &user,
            r#"
            [local]
            cpu_quota_pct = 150
            memory_max = "12G"
            "#,
        )
        .unwrap();

        let result = Config::load_with_paths(None, Some(&user), None, Some(&state));

        assert!(result.broken_banner.is_none(), "clean load: no banner");
        assert_eq!(result.config.local.cpu_quota_pct, 150);
        assert!(state.join("last-known-good.toml").exists(), "snapshot");
        assert!(
            state.join("last-known-good.meta.json").exists(),
            "snapshot metadata"
        );
        assert!(!state.join("broken-config.marker").exists());
    }

    /// The Q-7 acceptance pair: a corrupted config serves the persisted
    /// last-known-good snapshot with a broken-config banner — degraded
    /// service, but never silence and never a hard failure.
    #[test]
    fn lkg_corrupted_config_serves_snapshot_with_banner() {
        let temp = TempDir::new().unwrap();
        let state = temp.path().join("state");
        let user = temp.path().join("config.toml");

        // First run: healthy config loads clean and (exactly what load()
        // does on success) leaves a snapshot of itself behind.
        fs::write(
            &user,
            r#"
            [local]
            cpu_quota_pct = 150
            memory_max = "12G"
            "#,
        )
        .unwrap();
        let good = Config::load_with_paths(None, Some(&user), None, Some(&state));
        assert!(good.broken_banner.is_none());

        // Second run: the user config has since been corrupted.
        fs::write(&user, CORRUPT_TOML).unwrap();
        let degraded = Config::load_with_paths(None, Some(&user), None, Some(&state));

        // The snapshot is served, not Tier-0 defaults: every value it
        // captured stays in force.
        assert_eq!(degraded.config.local.cpu_quota_pct, 150);
        assert_eq!(degraded.config.local.memory_max, "12G");

        // And the degradation is loud: banner shown, first detection = 1.
        let banner = degraded.broken_banner.expect("broken banner shown");
        assert_eq!(banner.count, 1);
    }

    /// The banner escalates: each consecutive degraded run increments the
    /// count while "broken since" stays pinned to the first detection.
    #[test]
    fn lkg_banner_count_escalates_across_degraded_runs() {
        let temp = TempDir::new().unwrap();
        let state = temp.path().join("state");
        let user = temp.path().join("config.toml");
        fs::write(&user, CORRUPT_TOML).unwrap();

        let mut first_since = None;
        for expected in 1..=3u64 {
            let result = Config::load_with_paths(None, Some(&user), None, Some(&state));
            let banner = result
                .broken_banner
                .unwrap_or_else(|| panic!("run {expected}: banner missing"));
            assert_eq!(banner.count, expected, "run {expected}");
            match first_since {
                None => first_since = Some(banner.since),
                Some(prev) => assert_eq!(banner.since, prev, "since must not drift"),
            }
        }
    }

    /// With no snapshot on disk, a broken config fails open to Tier-0
    /// defaults — but still banners (plan Q-7: "plain passthrough + banner
    /// if no snapshot exists (never silent)").
    #[test]
    fn lkg_no_snapshot_falls_back_to_tier0_with_banner() {
        let temp = TempDir::new().unwrap();
        let state = temp.path().join("state"); // never populated
        let user = temp.path().join("config.toml");
        fs::write(&user, CORRUPT_TOML).unwrap();

        let result = Config::load_with_paths(None, Some(&user), None, Some(&state));

        assert_eq!(result.config, Config::tier_0_defaults());
        let banner = result
            .broken_banner
            .expect("banner even without a snapshot");
        assert_eq!(banner.count, 1);

        // With no state dir at all there is nowhere to track escalation, so
        // the banner struct is None — the stderr line is then the only
        // signal, which is why load_with_paths prints before falling back.
        let result = Config::load_with_paths(None, Some(&user), None, None);
        assert_eq!(result.config, Config::tier_0_defaults());
        assert!(result.broken_banner.is_none());
    }

    /// A fix stops the banner and resets escalation: the clean load removes
    /// the marker and refreshes the snapshot, so a later corruption starts a
    /// fresh count and serves the fixed config — not the pre-fix snapshot.
    #[test]
    fn lkg_clean_load_resets_broken_state() {
        let temp = TempDir::new().unwrap();
        let state = temp.path().join("state");
        let user = temp.path().join("config.toml");
        let marker = state.join("broken-config.marker");

        // Break, then verify the marker exists (escalation state on disk).
        fs::write(&user, CORRUPT_TOML).unwrap();
        let broken = Config::load_with_paths(None, Some(&user), None, Some(&state));
        assert_eq!(broken.broken_banner.as_ref().unwrap().count, 1);
        assert!(marker.exists(), "marker written on first detection");

        // Fix: the banner stops and the marker is retired.
        fs::write(
            &user,
            r#"
            [local]
            cpu_quota_pct = 90
            "#,
        )
        .unwrap();
        let fixed = Config::load_with_paths(None, Some(&user), None, Some(&state));
        assert!(fixed.broken_banner.is_none(), "fix stops the banner");
        assert!(!marker.exists(), "fix clears the marker");

        // Break again: fresh incident — count restarts at 1, and the
        // snapshot served is the fixed config (cpu_quota_pct = 90).
        fs::write(&user, CORRUPT_TOML).unwrap();
        let again = Config::load_with_paths(None, Some(&user), None, Some(&state));
        let banner = again.broken_banner.expect("second incident banner");
        assert_eq!(banner.count, 1, "escalation restarted");
        assert_eq!(again.config.local.cpu_quota_pct, 90, "refreshed snapshot");
    }

    /// An unusable snapshot (corrupt metadata) degrades to Tier-0 with a
    /// banner rather than panicking or serving silence. The marker is still
    /// written, so repeated runs keep escalating.
    #[test]
    fn lkg_unusable_snapshot_degrades_to_tier0_with_banner() {
        let temp = TempDir::new().unwrap();
        let state = temp.path().join("state");
        let user = temp.path().join("config.toml");

        // Produce a snapshot, then maim its metadata.
        fs::write(&user, "[local]\ncpu_quota_pct = 150\n").unwrap();
        Config::load_with_paths(None, Some(&user), None, Some(&state));
        fs::write(state.join("last-known-good.meta.json"), "{oops").unwrap();

        fs::write(&user, CORRUPT_TOML).unwrap();
        let result = Config::load_with_paths(None, Some(&user), None, Some(&state));

        assert_eq!(result.config, Config::tier_0_defaults());
        let banner = result.broken_banner.expect("banner on unusable snapshot");
        assert_eq!(banner.count, 1);
    }

    /// A corrupt or unreadable marker must not suppress the banner: the run
    /// is treated as a fresh detection and the marker is rewritten into a
    /// readable shape so the next run can escalate from it.
    #[test]
    fn lkg_corrupt_marker_still_banners_as_fresh_detection() {
        let temp = TempDir::new().unwrap();
        let state = temp.path().join("state");
        let user = temp.path().join("config.toml");

        // Healthy config -> snapshot; then corrupt both the config and the
        // escalation marker.
        fs::write(&user, "[local]\ncpu_quota_pct = 150\n").unwrap();
        Config::load_with_paths(None, Some(&user), None, Some(&state));
        fs::write(&user, CORRUPT_TOML).unwrap();
        fs::write(state.join("broken-config.marker"), "not json {").unwrap();

        let result = Config::load_with_paths(None, Some(&user), None, Some(&state));

        // The snapshot is still served and the banner still shown.
        assert_eq!(result.config.local.cpu_quota_pct, 150);
        let banner = result.broken_banner.expect("banner despite corrupt marker");
        assert_eq!(banner.count, 1);

        // The marker was rewritten readably, ready to escalate.
        let marker: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(state.join("broken-config.marker")).unwrap())
                .expect("marker rewritten as JSON");
        assert_eq!(marker["count"], 1);
    }
}
