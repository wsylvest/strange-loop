//! Configuration loader for `strange-loop.toml`.
//!
//! Defaults are hard-coded here so a missing or partial config file
//! still produces a runnable agent in M0. As the build progresses and
//! more subsystems come online, fields are added and defaults are
//! revisited — see `docs/SYSTEM_SPEC.md` §11 for the eventual shape.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The top-level config. Loaded from `strange-loop.toml` next to the
/// binary or at a path passed via `--config`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub agent: AgentConfig,
    pub governance: GovernanceConfig,
    pub llm: LlmConfig,
    pub budget: BudgetConfig,
    #[serde(rename = "loop")]
    pub tool_loop: LoopConfig,
    pub git: GitConfig,
    pub consciousness: ConsciousnessConfig,
    pub isolation: IsolationConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub name: String,
    pub owner_id: String,
    pub repo_root: PathBuf,
    pub data_dir: PathBuf,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: "strange-loop".to_string(),
            owner_id: String::new(),
            repo_root: PathBuf::from("."),
            data_dir: PathBuf::from("./data"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GovernanceConfig {
    pub charter: PathBuf,
    pub creed: PathBuf,
    pub doctrine: PathBuf,
    pub scratch: PathBuf,
    pub journal_dir: PathBuf,
    pub protected: Vec<PathBuf>,
}

impl Default for GovernanceConfig {
    fn default() -> Self {
        Self {
            charter: PathBuf::from("prompts/CHARTER.md"),
            creed: PathBuf::from("prompts/CREED.md"),
            doctrine: PathBuf::from("prompts/doctrine.toml"),
            scratch: PathBuf::from("prompts/scratch.md"),
            journal_dir: PathBuf::from("journal"),
            protected: vec![
                PathBuf::from("prompts/CHARTER.md"),
                PathBuf::from("prompts/CREED.md"),
                PathBuf::from("journal/"),
                PathBuf::from(".git/"),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub provider: String,
    pub api_key_env: String,
    pub default_model: String,
    pub light_model: String,
    pub code_model: String,
    pub fallback_chain: Vec<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: "openrouter".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            default_model: "anthropic/claude-sonnet-4.6".to_string(),
            light_model: "google/gemini-3-pro-preview".to_string(),
            code_model: "anthropic/claude-sonnet-4.6".to_string(),
            fallback_chain: vec![
                "anthropic/claude-sonnet-4.6".to_string(),
                "google/gemini-2.5-pro-preview".to_string(),
                "openai/gpt-4.1".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    pub total_usd: f64,
    pub bg_pct: u32,
    pub drift_check_every: u32,
    pub hard_task_pct: f64,
    pub soft_task_pct: f64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            total_usd: 100.0,
            bg_pct: 10,
            drift_check_every: 50,
            hard_task_pct: 0.50,
            soft_task_pct: 0.30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoopConfig {
    pub max_rounds: u32,
    pub self_check_interval: u32,
    pub tool_result_max_chars: u32,
    pub context_soft_cap_tokens: u32,
    pub parallel_readonly: bool,
    pub max_concurrent_tasks: u32,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            max_rounds: 200,
            self_check_interval: 50,
            tool_result_max_chars: 15_000,
            context_soft_cap_tokens: 150_000,
            parallel_readonly: true,
            // v0.1 default is 2 per SYSTEM_SPEC §6.1 — low enough to be
            // safe, high enough to actually exercise the parallel path.
            max_concurrent_tasks: 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitConfig {
    pub dev_branch: String,
    pub protected_branches: Vec<String>,
    pub stable_tag_prefix: String,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            dev_branch: "agent".to_string(),
            protected_branches: vec![
                "main".to_string(),
                "master".to_string(),
                "release/*".to_string(),
            ],
            stable_tag_prefix: "stable-".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsciousnessConfig {
    pub enabled: bool,
    pub default_wakeup_sec: u32,
    pub max_rounds: u32,
    pub proactive_message_rate_per_hour: u32,
}

impl Default for ConsciousnessConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            default_wakeup_sec: 300,
            max_rounds: 5,
            proactive_message_rate_per_hour: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IsolationConfig {
    /// "auto" | "apple" | "firecracker" | "docker"
    pub cell_backend: String,
    /// "workerd" | "disabled"
    pub edge_backend: String,
}

impl Default for IsolationConfig {
    fn default() -> Self {
        Self {
            cell_backend: "auto".to_string(),
            edge_backend: "workerd".to_string(),
        }
    }
}

impl Config {
    /// Load from a TOML file. Missing file → returns default config.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            tracing::debug!(path = %path.display(), "no config file; using defaults");
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {:?}", path))?;
        let cfg: Self = toml::from_str(&text)
            .with_context(|| format!("parsing config file {:?}", path))?;
        Ok(cfg)
    }

    /// Resolve the cell backend. If `cell_backend = "auto"`, pick per platform.
    /// See `docs/SYSTEM_SPEC.md` §3.1 for the defaults-by-platform rationale.
    pub fn resolved_cell_backend(&self) -> CellBackend {
        if self.isolation.cell_backend == "auto" {
            if cfg!(target_os = "macos") {
                CellBackend::Apple
            } else if cfg!(target_os = "linux") {
                // We cannot probe /dev/kvm from a pure-const context;
                // the runtime does a runtime check in M2 when the Cell
                // tier actually runs. For now, "auto on Linux" reports
                // firecracker as the preferred backend.
                CellBackend::Firecracker
            } else {
                CellBackend::Docker
            }
        } else {
            match self.isolation.cell_backend.as_str() {
                "apple" => CellBackend::Apple,
                "firecracker" => CellBackend::Firecracker,
                "docker" => CellBackend::Docker,
                _ => CellBackend::Docker,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellBackend {
    Apple,
    Firecracker,
    Docker,
}

impl CellBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Apple => "apple",
            Self::Firecracker => "firecracker",
            Self::Docker => "docker",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sensible() {
        let cfg = Config::default();
        assert_eq!(cfg.tool_loop.max_concurrent_tasks, 2);
        assert_eq!(cfg.tool_loop.max_rounds, 200);
        assert_eq!(cfg.budget.total_usd, 100.0);
        assert_eq!(cfg.git.dev_branch, "agent");
        assert!(cfg
            .git
            .protected_branches
            .iter()
            .any(|b| b == "main"));
    }

    #[test]
    fn missing_config_file_returns_defaults() {
        let cfg = Config::load("/nonexistent/strange-loop.toml").unwrap();
        assert_eq!(cfg.agent.name, "strange-loop");
    }

    #[test]
    fn load_roundtrip_from_toml_text() {
        let toml_text = r#"
            [agent]
            name = "test-loop"
            owner_id = "alice"

            [budget]
            total_usd = 50.0
        "#;
        let cfg: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(cfg.agent.name, "test-loop");
        assert_eq!(cfg.agent.owner_id, "alice");
        assert_eq!(cfg.budget.total_usd, 50.0);
        // defaults still populated for unspecified sections
        assert_eq!(cfg.tool_loop.max_concurrent_tasks, 2);
    }

    #[test]
    fn cell_backend_auto_resolves_per_platform() {
        let cfg = Config::default();
        let backend = cfg.resolved_cell_backend();
        if cfg!(target_os = "macos") {
            assert_eq!(backend, CellBackend::Apple);
        } else if cfg!(target_os = "linux") {
            assert_eq!(backend, CellBackend::Firecracker);
        } else {
            assert_eq!(backend, CellBackend::Docker);
        }
    }

    #[test]
    fn cell_backend_explicit_docker() {
        let mut cfg = Config::default();
        cfg.isolation.cell_backend = "docker".to_string();
        assert_eq!(cfg.resolved_cell_backend(), CellBackend::Docker);
    }
}
