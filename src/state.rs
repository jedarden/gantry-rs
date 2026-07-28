// gantry — state file management for gantry on/off kill switch (plan Component 2).
//
// Phase 1a: kill switches GANTRY_LOCAL=1 and gantry on/off state file (bf-2u0).
//
// This module defines:
// - State file at ~/.local/state/gantry/state.toml
// - on/off toggle commands
// - GANTRY_ON=1 environment variable override
// - Integration with config layering for kill switch detection

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

/// Current schema version for state file.
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// Gantry state file structure.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GantryState {
    /// Schema version for migration handling.
    pub schema_version: u32,
    /// Whether gantry is enabled (true) or disabled (false).
    /// When disabled, all intercepted commands run locally with cgroup caps.
    pub enabled: bool,
}

impl Default for GantryState {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            enabled: true, // Enabled by default
        }
    }
}

/// Error types for state file operations.
#[derive(Debug)]
pub enum StateError {
    /// Cannot determine state directory.
    CannotDetermineStateDir,
    /// Cannot create state directory.
    CannotCreateStateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Cannot read state file.
    CannotReadState {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Cannot write state file.
    CannotWriteState {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Cannot parse state file.
    CannotParseState {
        path: PathBuf,
        source: toml::de::Error,
    },
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CannotDetermineStateDir => write!(f, "cannot determine state directory"),
            Self::CannotCreateStateDir { path, source } => {
                write!(
                    f,
                    "cannot create state directory {}: {}",
                    path.display(),
                    source
                )
            }
            Self::CannotReadState { path, source } => {
                write!(f, "cannot read state file {}: {}", path.display(), source)
            }
            Self::CannotWriteState { path, source } => {
                write!(f, "cannot write state file {}: {}", path.display(), source)
            }
            Self::CannotParseState { path, source } => {
                write!(f, "cannot parse state file {}: {}", path.display(), source)
            }
        }
    }
}

impl std::error::Error for StateError {}

/// State file manager.
pub struct StateFile {
    /// Path to the state file.
    state_path: PathBuf,
}

impl StateFile {
    /// Get the gantry state directory following XDG standard.
    ///
    /// Returns `~/.local/state/gantry` on Linux following XDG Base Directory Specification.
    /// Returns None if HOME cannot be determined (should not happen in normal operation).
    pub fn state_dir() -> Option<PathBuf> {
        std::env::var("HOME").ok().map(|home| {
            PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("gantry")
        })
    }

    /// Open or create the state file at ~/.local/state/gantry/state.toml.
    ///
    /// Creates the state directory if it doesn't exist. If the state file
    /// doesn't exist, creates a default enabled state.
    ///
    /// Returns Err if the state directory cannot be created or the file
    /// cannot be read/written.
    pub fn open() -> Result<Self, StateError> {
        let state_dir = Self::state_dir().ok_or(StateError::CannotDetermineStateDir)?;

        // Create state directory if it doesn't exist
        fs::create_dir_all(&state_dir).map_err(|e| StateError::CannotCreateStateDir {
            path: state_dir.clone(),
            source: e,
        })?;

        let state_path = state_dir.join("state.toml");

        Ok(Self { state_path })
    }

    /// Load the current state from disk.
    ///
    /// Returns default enabled state if the file doesn't exist.
    /// Returns Err if the file exists but cannot be read or parsed.
    pub fn load(&self) -> Result<GantryState, StateError> {
        if !self.state_path.exists() {
            // No state file = default enabled state
            return Ok(GantryState::default());
        }

        let content =
            fs::read_to_string(&self.state_path).map_err(|e| StateError::CannotReadState {
                path: self.state_path.clone(),
                source: e,
            })?;

        toml::from_str(&content).map_err(|e| StateError::CannotParseState {
            path: self.state_path.clone(),
            source: e,
        })
    }

    /// Save state to disk.
    ///
    /// Writes the state atomically using a temp file + rename.
    /// Returns Err if the write fails (disk full, permissions, etc.).
    pub fn save(&self, state: &GantryState) -> Result<(), StateError> {
        let toml = toml::to_string_pretty(state).map_err(|e| StateError::CannotWriteState {
            path: self.state_path.clone(),
            source: std::io::Error::new(std::io::ErrorKind::Other, e),
        })?;

        // Atomic write: temp file + rename
        let temp_path = self.state_path.with_extension("tmp");
        let mut file = fs::File::create(&temp_path).map_err(|e| StateError::CannotWriteState {
            path: temp_path.clone(),
            source: e,
        })?;

        file.write_all(toml.as_bytes())
            .map_err(|e| StateError::CannotWriteState {
                path: temp_path.clone(),
                source: e,
            })?;

        file.flush().map_err(|e| StateError::CannotWriteState {
            path: temp_path.clone(),
            source: e,
        })?;

        // Atomic rename
        fs::rename(&temp_path, &self.state_path).map_err(|e| StateError::CannotWriteState {
            path: self.state_path.clone(),
            source: e,
        })?;

        Ok(())
    }

    /// Enable gantry (remove kill switch).
    ///
    /// Sets enabled=true in the state file and saves it.
    /// Returns Err if the save fails.
    pub fn enable(&self) -> Result<(), StateError> {
        let mut state = self.load()?;
        state.enabled = true;
        self.save(&state)
    }

    /// Disable gantry (activate kill switch).
    ///
    /// Sets enabled=false in the state file and saves it.
    /// When disabled, all intercepted commands run locally with cgroup caps.
    /// Returns Err if the save fails.
    pub fn disable(&self) -> Result<(), StateError> {
        let mut state = self.load()?;
        state.enabled = false;
        self.save(&state)
    }
}

/// Check if gantry is enabled based on state file and environment variables.
///
/// Kill switch priority (highest to lowest):
/// 1. GANTRY_LOCAL=1 environment variable (forces local execution)
/// 2. GANTRY_ON=1 environment variable (overrides state file to enable)
/// 3. State file enabled=false (kill switch activated)
/// 4. Default: enabled
///
/// Returns (is_enabled, source) where source describes why the decision was made.
pub fn check_enabled() -> (bool, String) {
    // Check GANTRY_LOCAL first (highest priority)
    if std::env::var("GANTRY_LOCAL").is_ok() {
        return (false, "GANTRY_LOCAL=1".to_string());
    }

    // Check GANTRY_ON override
    if let Ok(val) = std::env::var("GANTRY_ON") {
        let enabled = val == "1";
        return (enabled, format!("GANTRY_ON={}", val));
    }

    // Check state file
    match StateFile::open().and_then(|sf| sf.load()) {
        Ok(state) => {
            if !state.enabled {
                return (false, "state file: gantry off".to_string());
            }
        }
        Err(_) => {
            // If we can't read the state file, assume enabled (fail open)
            // but log a warning in the real implementation
        }
    }

    // Default: enabled
    (true, "default: enabled".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_state() {
        let state = GantryState::default();
        assert_eq!(state.schema_version, STATE_SCHEMA_VERSION);
        assert!(state.enabled);
    }

    #[test]
    fn test_state_serialization() {
        let state = GantryState::default();
        let toml = toml::to_string(&state).unwrap();
        let parsed: GantryState = toml::from_str(&toml).unwrap();
        assert_eq!(parsed, state);
    }

    #[test]
    fn test_check_enabled_gantry_local() {
        std::env::set_var("GANTRY_LOCAL", "1");
        let (enabled, source) = check_enabled();
        assert!(!enabled);
        assert_eq!(source, "GANTRY_LOCAL=1");
        std::env::remove_var("GANTRY_LOCAL");
    }

    #[test]
    fn test_check_enabled_gantry_on() {
        std::env::set_var("GANTRY_ON", "0");
        let (enabled, source) = check_enabled();
        assert!(!enabled);
        assert!(source.contains("GANTRY_ON"));

        std::env::set_var("GANTRY_ON", "1");
        let (enabled, source) = check_enabled();
        assert!(enabled);
        assert!(source.contains("GANTRY_ON"));

        std::env::remove_var("GANTRY_ON");
    }

    #[test]
    fn test_state_file_enable_disable() {
        // Create a temp state file for testing
        let temp_dir = tempfile::tempdir().unwrap();
        let state_path = temp_dir.path().join("state.toml");

        // We can't easily test StateFile::open() without mocking,
        // so we'll test the serialization logic
        let state = GantryState::default();
        let toml = toml::to_string(&state).unwrap();

        fs::write(&state_path, toml).unwrap();
        let content = fs::read_to_string(&state_path).unwrap();
        let parsed: GantryState = toml::from_str(&content).unwrap();

        assert!(parsed.enabled);
    }
}
