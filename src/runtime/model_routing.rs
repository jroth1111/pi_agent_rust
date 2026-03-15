use crate::runtime::types::{ModelProfile, ModelSelector, RunPhase, TaskId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeModelRef {
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
}

impl From<&ModelSelector> for RuntimeModelRef {
    fn from(value: &ModelSelector) -> Self {
        Self {
            provider: value.provider.clone(),
            model: value.model.clone(),
            thinking_level: value.thinking_level.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelRoute {
    pub planner: RuntimeModelRef,
    pub executor: RuntimeModelRef,
    pub verifier: RuntimeModelRef,
    pub summarizer: RuntimeModelRef,
    pub background: RuntimeModelRef,
}

impl ModelRoute {
    pub fn from_profile(profile: &ModelProfile) -> Self {
        Self {
            planner: RuntimeModelRef::from(&profile.planner),
            executor: RuntimeModelRef::from(&profile.executor),
            verifier: RuntimeModelRef::from(&profile.verifier),
            summarizer: RuntimeModelRef::from(&profile.summarizer),
            background: RuntimeModelRef::from(&profile.background),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RouteRequest {
    pub phase: RunPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum RouteError {
    #[error("no route configured for phase {0:?}")]
    MissingRoute(RunPhase),
}

pub trait ModelRouter {
    fn route(&self, request: &RouteRequest) -> Result<ModelRoute, RouteError>;
}

#[derive(Debug, Clone)]
pub struct PhaseModelRouter {
    default_route: ModelRoute,
    overrides: HashMap<RunPhase, ModelRoute>,
}

impl PhaseModelRouter {
    pub fn new(default_route: ModelRoute) -> Self {
        Self {
            default_route,
            overrides: HashMap::new(),
        }
    }

    pub fn insert(&mut self, phase: RunPhase, route: ModelRoute) -> Option<ModelRoute> {
        self.overrides.insert(phase, route)
    }

    pub fn route_for_phase(&self, phase: RunPhase) -> Result<ModelRoute, RouteError> {
        if phase.is_terminal() {
            return Err(RouteError::MissingRoute(phase));
        }
        Ok(self
            .overrides
            .get(&phase)
            .cloned()
            .unwrap_or_else(|| self.default_route.clone()))
    }
}

impl ModelRouter for PhaseModelRouter {
    fn route(&self, request: &RouteRequest) -> Result<ModelRoute, RouteError> {
        self.route_for_phase(request.phase)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_profile() -> ModelProfile {
        ModelProfile {
            planner: ModelSelector {
                provider: "anthropic".to_string(),
                model: "planner".to_string(),
                thinking_level: Some("high".to_string()),
            },
            executor: ModelSelector {
                provider: "openai".to_string(),
                model: "executor".to_string(),
                thinking_level: None,
            },
            verifier: ModelSelector {
                provider: "openai".to_string(),
                model: "verifier".to_string(),
                thinking_level: None,
            },
            summarizer: ModelSelector {
                provider: "openai".to_string(),
                model: "summary".to_string(),
                thinking_level: None,
            },
            background: ModelSelector {
                provider: "openai".to_string(),
                model: "background".to_string(),
                thinking_level: None,
            },
        }
    }

    #[test]
    fn router_uses_default_route_when_no_override_exists() {
        let route = ModelRoute::from_profile(&sample_profile());
        let router = PhaseModelRouter::new(route.clone());
        let selected = router
            .route(&RouteRequest {
                phase: RunPhase::Planning,
                task_id: None,
            })
            .expect("route");
        assert_eq!(selected, route);
    }

    #[test]
    fn router_rejects_terminal_phases() {
        let route = ModelRoute::from_profile(&sample_profile());
        let router = PhaseModelRouter::new(route);
        let err = router
            .route_for_phase(RunPhase::Completed)
            .expect_err("terminal");
        assert_eq!(err, RouteError::MissingRoute(RunPhase::Completed));
    }
}
