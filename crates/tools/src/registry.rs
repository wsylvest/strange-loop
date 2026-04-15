//! Tool registry. Owns the catalog and produces `ToolSchema` lists for
//! the LLM client. Selective-schema mode (core-only) is used when
//! building the LLM context per SYSTEM_SPEC §6.5.

use std::collections::BTreeMap;
use std::sync::Arc;

use sl_llm::ToolSchema;

use crate::tool::Tool;

/// The registry. Cheap to clone (everything is `Arc`).
#[derive(Clone, Default)]
pub struct Registry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool. If a tool with the same name already exists,
    /// the new one replaces it (the dispatcher will use the new one).
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.tools.insert(name, tool);
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// All registered tool names, sorted.
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    /// Number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// True if no tools are registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Build the `ToolSchema` list for the LLM. If `core_only` is true,
    /// excludes tools with `is_core() == false`. The discovery meta-tools
    /// (`list_available_tools`, `enable_tools`) come into play in M3+;
    /// in M1 we simply load the full set when called for `core_only`.
    pub fn schemas(&self, core_only: bool) -> Vec<ToolSchema> {
        self.tools
            .values()
            .filter(|t| !core_only || t.is_core())
            .map(|t| t.schema())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{HostClass, Tool, ToolCtx, ToolResult};
    use async_trait::async_trait;
    use serde_json::json;
    use sl_llm::ToolSchema;
    use std::sync::Arc;

    struct FakeTool {
        name: &'static str,
        core: bool,
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn name(&self) -> &str {
            self.name
        }
        fn schema(&self) -> ToolSchema {
            ToolSchema::new(self.name, "fake", json!({"type":"object","properties":{}}))
        }
        fn host_class(&self) -> HostClass {
            HostClass::InProc
        }
        fn is_core(&self) -> bool {
            self.core
        }
        async fn invoke(&self, _ctx: &ToolCtx, _args: serde_json::Value) -> ToolResult {
            Ok("ok".into())
        }
    }

    #[test]
    fn register_and_lookup() {
        let mut reg = Registry::new();
        reg.register(Arc::new(FakeTool {
            name: "alpha",
            core: true,
        }));
        reg.register(Arc::new(FakeTool {
            name: "beta",
            core: false,
        }));
        assert_eq!(reg.len(), 2);
        assert!(reg.get("alpha").is_some());
        assert!(reg.get("missing").is_none());
        let names = reg.names();
        assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn core_only_filters_non_core() {
        let mut reg = Registry::new();
        reg.register(Arc::new(FakeTool {
            name: "alpha",
            core: true,
        }));
        reg.register(Arc::new(FakeTool {
            name: "beta",
            core: false,
        }));
        let all = reg.schemas(false);
        let core = reg.schemas(true);
        assert_eq!(all.len(), 2);
        assert_eq!(core.len(), 1);
        assert_eq!(core[0].name, "alpha");
    }
}
