use crate::runtime::reliability::state::FailureClass;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReliabilityEdge {
    pub id: String,
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EdgeKind {
    Prerequisite { trigger: EdgeTrigger },
    Hierarchy { relation: HierarchyRelation },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "trigger", rename_all = "snake_case")]
pub enum EdgeTrigger {
    OnSuccess,
    OnFailure(FailureClassMask),
    Always,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HierarchyRelation {
    ParentOf,
    ChildOf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FailureClassMask {
    pub classes: Vec<FailureClass>,
}

impl FailureClassMask {
    pub fn matches(&self, class: FailureClass) -> bool {
        self.classes.contains(&class)
    }

    pub const fn from_classes(classes: Vec<FailureClass>) -> Self {
        Self { classes }
    }
}
