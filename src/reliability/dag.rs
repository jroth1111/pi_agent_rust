use crate::reliability::edge::{EdgeKind, EdgeTrigger, ReliabilityEdge};
use crate::reliability::state::{RuntimeState, TerminalState};
use crate::reliability::task::TaskNode;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default)]
pub struct DagEvaluation {
    pub promoted_to_ready: Vec<String>,
    pub updated_blocked_waiting_on: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DagValidation {
    pub has_cycle: bool,
    pub unknown_node_refs: Vec<String>,
    pub orphaned_task_ids: Vec<String>,
}

pub struct DagEvaluator;

impl DagEvaluator {
    pub fn evaluate_and_unblock(
        tasks: &mut [TaskNode],
        edges: &[ReliabilityEdge],
    ) -> DagEvaluation {
        let mut result = DagEvaluation::default();

        let terminal_states: HashMap<String, TerminalState> = tasks
            .iter()
            .filter_map(|task| match &task.runtime.state {
                RuntimeState::Terminal(term) => Some((task.id.clone(), term.clone())),
                _ => None,
            })
            .collect();

        for task in tasks.iter_mut() {
            let RuntimeState::Blocked { waiting_on } = &task.runtime.state else {
                continue;
            };

            let prerequisite_edges = edges.iter().filter(|edge| {
                edge.to == task.id && matches!(edge.kind, EdgeKind::Prerequisite { .. })
            });

            let mut all_satisfied = true;
            let mut still_waiting_on = Vec::new();

            for edge in prerequisite_edges {
                let EdgeKind::Prerequisite { ref trigger } = edge.kind else {
                    continue;
                };

                match terminal_states.get(&edge.from) {
                    Some(terminal) if Self::trigger_satisfied(trigger, terminal) => {}
                    _ => {
                        all_satisfied = false;
                        still_waiting_on.push(edge.from.clone());
                    }
                }
            }

            if all_satisfied {
                task.runtime.state = RuntimeState::Ready;
                result.promoted_to_ready.push(task.id.clone());
            } else if *waiting_on != still_waiting_on {
                task.runtime.state = RuntimeState::Blocked {
                    waiting_on: still_waiting_on,
                };
                result.updated_blocked_waiting_on.push(task.id.clone());
            }
        }

        result
    }

    pub fn detect_cycle(edges: &[ReliabilityEdge]) -> bool {
        let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut nodes: HashSet<&str> = HashSet::new();

        for edge in edges {
            if matches!(edge.kind, EdgeKind::Prerequisite { .. }) {
                adjacency
                    .entry(edge.from.as_str())
                    .or_default()
                    .push(edge.to.as_str());
                nodes.insert(edge.from.as_str());
                nodes.insert(edge.to.as_str());
            }
        }

        let mut visiting = HashSet::new();
        let mut visited = HashSet::new();

        for node in nodes {
            if Self::dfs_has_cycle(node, &adjacency, &mut visiting, &mut visited) {
                return true;
            }
        }
        false
    }

    pub fn validate(tasks: &[TaskNode], edges: &[ReliabilityEdge]) -> DagValidation {
        let task_ids = tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<HashSet<_>>();
        let mut unknown_refs = Vec::new();
        let mut touching_nodes = HashSet::new();

        for edge in edges {
            if matches!(edge.kind, EdgeKind::Prerequisite { .. }) {
                touching_nodes.insert(edge.from.as_str());
                touching_nodes.insert(edge.to.as_str());
                if !task_ids.contains(edge.from.as_str()) {
                    unknown_refs.push(format!("{}:from={}", edge.id, edge.from));
                }
                if !task_ids.contains(edge.to.as_str()) {
                    unknown_refs.push(format!("{}:to={}", edge.id, edge.to));
                }
            }
        }

        let orphaned = tasks
            .iter()
            .filter(|task| {
                !matches!(task.runtime.state, RuntimeState::Terminal(_))
                    && !touching_nodes.contains(task.id.as_str())
                    && tasks.len() > 1
            })
            .map(|task| task.id.clone())
            .collect::<Vec<_>>();

        unknown_refs.sort();
        unknown_refs.dedup();

        DagValidation {
            has_cycle: Self::detect_cycle(edges),
            unknown_node_refs: unknown_refs,
            orphaned_task_ids: orphaned,
        }
    }

    fn dfs_has_cycle<'a>(
        node: &'a str,
        adjacency: &HashMap<&'a str, Vec<&'a str>>,
        visiting: &mut HashSet<&'a str>,
        visited: &mut HashSet<&'a str>,
    ) -> bool {
        if visited.contains(node) {
            return false;
        }
        if !visiting.insert(node) {
            return true;
        }

        if let Some(neighbors) = adjacency.get(node) {
            for neighbor in neighbors {
                if Self::dfs_has_cycle(neighbor, adjacency, visiting, visited) {
                    return true;
                }
            }
        }

        visiting.remove(node);
        visited.insert(node);
        false
    }

    fn trigger_satisfied(trigger: &EdgeTrigger, terminal: &TerminalState) -> bool {
        match trigger {
            EdgeTrigger::Always => true,
            EdgeTrigger::OnSuccess => matches!(terminal, TerminalState::Succeeded { .. }),
            EdgeTrigger::OnFailure(mask) => match terminal {
                TerminalState::Failed { class, .. } => mask.matches(*class),
                _ => false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reliability::edge::{EdgeKind, EdgeTrigger, FailureClassMask, ReliabilityEdge};
    use crate::reliability::state::{FailureClass, RuntimeState, TerminalState};
    use crate::reliability::task::{TaskConstraintSet, TaskNode, TaskSpec, VerifyPlan};
    use chrono::Utc;

    fn task(id: &str, state: RuntimeState) -> TaskNode {
        TaskNode {
            id: id.to_string(),
            spec: TaskSpec {
                objective: "obj".to_string(),
                constraints: TaskConstraintSet::default(),
                verify: VerifyPlan::Standard {
                    audit_diff: true,
                    command: "echo ok".to_string(),
                    timeout_sec: 10,
                },
                max_attempts: 3,
                input_snapshot: "abc".to_string(),
            },
            runtime: crate::reliability::task::TaskRuntime {
                state,
                attempt: 0,
                last_transition_at: Utc::now(),
            },
            created_at: Utc::now(),
        }
    }

    #[test]
    fn dag_promotes_blocked_when_dependencies_satisfied() {
        let mut tasks = vec![
            task(
                "a",
                RuntimeState::Terminal(TerminalState::Succeeded {
                    patch_digest: "x".into(),
                    verify_run_id: "vr".into(),
                    completed_at: Utc::now(),
                }),
            ),
            task(
                "b",
                RuntimeState::Blocked {
                    waiting_on: vec!["a".into()],
                },
            ),
        ];

        let edges = vec![ReliabilityEdge {
            id: "e1".into(),
            from: "a".into(),
            to: "b".into(),
            kind: EdgeKind::Prerequisite {
                trigger: EdgeTrigger::OnSuccess,
            },
        }];

        let out = DagEvaluator::evaluate_and_unblock(&mut tasks, &edges);
        assert_eq!(out.promoted_to_ready, vec!["b".to_string()]);
        assert!(matches!(tasks[1].runtime.state, RuntimeState::Ready));
    }

    #[test]
    fn dag_unblocks_ready_tasks() {
        let mut tasks = vec![
            task(
                "task-a",
                RuntimeState::Terminal(TerminalState::Succeeded {
                    patch_digest: "digest-a".into(),
                    verify_run_id: "vr-a".into(),
                    completed_at: Utc::now(),
                }),
            ),
            task(
                "task-b",
                RuntimeState::Blocked {
                    waiting_on: vec!["task-a".into()],
                },
            ),
        ];
        let edges = vec![ReliabilityEdge {
            id: "edge-a-b".into(),
            from: "task-a".into(),
            to: "task-b".into(),
            kind: EdgeKind::Prerequisite {
                trigger: EdgeTrigger::OnSuccess,
            },
        }];

        let out = DagEvaluator::evaluate_and_unblock(&mut tasks, &edges);
        assert_eq!(out.promoted_to_ready, vec!["task-b".to_string()]);
        assert!(matches!(tasks[1].runtime.state, RuntimeState::Ready));
    }

    #[test]
    fn dag_detects_cycle() {
        let edges = vec![
            ReliabilityEdge {
                id: "e1".into(),
                from: "a".into(),
                to: "b".into(),
                kind: EdgeKind::Prerequisite {
                    trigger: EdgeTrigger::Always,
                },
            },
            ReliabilityEdge {
                id: "e2".into(),
                from: "b".into(),
                to: "a".into(),
                kind: EdgeKind::Prerequisite {
                    trigger: EdgeTrigger::Always,
                },
            },
        ];
        assert!(DagEvaluator::detect_cycle(&edges));
    }

    #[test]
    fn dag_on_failure_mask_works() {
        let mut tasks = vec![
            task(
                "a",
                RuntimeState::Terminal(TerminalState::Failed {
                    class: FailureClass::VerificationFailed,
                    verify_run_id: Some("vr".into()),
                    failed_at: Utc::now(),
                }),
            ),
            task(
                "b",
                RuntimeState::Blocked {
                    waiting_on: vec!["a".into()],
                },
            ),
        ];
        let edges = vec![ReliabilityEdge {
            id: "e1".into(),
            from: "a".into(),
            to: "b".into(),
            kind: EdgeKind::Prerequisite {
                trigger: EdgeTrigger::OnFailure(FailureClassMask::from_classes(vec![
                    FailureClass::VerificationFailed,
                ])),
            },
        }];
        let out = DagEvaluator::evaluate_and_unblock(&mut tasks, &edges);
        assert_eq!(out.promoted_to_ready, vec!["b".to_string()]);
    }

    #[test]
    fn dag_validation_reports_unknown_refs_and_orphans() {
        let tasks = vec![
            task("a", RuntimeState::Ready),
            task("b", RuntimeState::Ready),
            task("c", RuntimeState::Ready),
        ];
        let edges = vec![ReliabilityEdge {
            id: "e1".into(),
            from: "a".into(),
            to: "missing".into(),
            kind: EdgeKind::Prerequisite {
                trigger: EdgeTrigger::Always,
            },
        }];

        let validation = DagEvaluator::validate(&tasks, &edges);
        assert!(!validation.has_cycle);
        assert_eq!(validation.unknown_node_refs.len(), 1);
        assert!(validation.orphaned_task_ids.contains(&"b".to_string()));
        assert!(validation.orphaned_task_ids.contains(&"c".to_string()));
    }
}
