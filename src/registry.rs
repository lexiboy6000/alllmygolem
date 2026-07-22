//! Holds all registered workflows and resolves dependency ordering.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{GolemError, Result};
use crate::workflow::{InputSpec, Workflow};

/// Serializable description of a workflow, sent to the GUI.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowInfo {
    pub name: String,
    pub description: String,
    pub dependencies: Vec<String>,
    pub run_after: Vec<String>,
    pub inputs: Vec<InputSpec>,
}

#[derive(Default)]
pub struct WorkflowRegistry {
    map: BTreeMap<&'static str, Arc<dyn Workflow>>,
}

impl WorkflowRegistry {
    pub fn new() -> Self {
        WorkflowRegistry {
            map: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, wf: Arc<dyn Workflow>) {
        self.map.insert(wf.name(), wf);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Workflow>> {
        self.map.get(name).cloned()
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.map.keys().copied().collect()
    }

    pub fn list(&self) -> Vec<WorkflowInfo> {
        self.map
            .values()
            .map(|w| WorkflowInfo {
                name: w.name().to_string(),
                description: w.description().to_string(),
                dependencies: w.dependencies().iter().map(|s| s.to_string()).collect(),
                run_after: w.run_after().iter().map(|s| s.to_string()).collect(),
                inputs: w.inputs(),
            })
            .collect()
    }

    /// Returns the names to run in order so that `target`'s dependencies run
    /// first, ending with `target`. Detects cycles and missing deps.
    pub fn resolve_order(&self, target: &str) -> Result<Vec<String>> {
        let mut order = Vec::new();
        let mut done = BTreeSet::new();
        let mut on_stack = BTreeSet::new();
        self.visit(target, &mut order, &mut done, &mut on_stack)?;
        Ok(order)
    }

    fn visit(
        &self,
        name: &str,
        order: &mut Vec<String>,
        done: &mut BTreeSet<String>,
        on_stack: &mut BTreeSet<String>,
    ) -> Result<()> {
        if done.contains(name) {
            return Ok(());
        }
        if !on_stack.insert(name.to_string()) {
            return Err(GolemError::Other(format!(
                "dependency cycle involving '{name}'"
            )));
        }
        let wf = self
            .get(name)
            .ok_or_else(|| GolemError::Other(format!("unknown workflow '{name}'")))?;
        for dep in wf.dependencies() {
            self.visit(dep, order, done, on_stack)?;
        }
        on_stack.remove(name);
        done.insert(name.to_string());
        order.push(name.to_string());
        Ok(())
    }
}
