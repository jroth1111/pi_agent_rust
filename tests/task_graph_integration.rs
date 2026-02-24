//! Integration tests for task graph DAG operations and recovery.
//!
//! Exercises the full workflow: build DAG -> validate ordering ->
//! simulate work -> unblock dependents -> handle recovery.

use pi::reliability::state::{RuntimeState, TerminalState};
use pi::task_graph::dag::{DependencyType, Edge, Trigger};
use pi::task_graph::recovery::{
    AgentRegistry, DeferredTask, check_deferred_tasks, expire_stale_leases,
};
use pi::task_graph::{Task, TaskGraph};

use chrono::{Duration, Utc};
use std::collections::{HashMap, HashSet};

/// Helper: build a graph with task IDs and edges.
fn build_graph(tasks: &[&str], edges: &[(&str, &str, DependencyType)]) -> TaskGraph {
    let mut graph = TaskGraph::new();
    for id in tasks {
        graph.add_task(Task::new(
            id.to_string(),
            format!("Task {id}"),
            String::new(),
        ));
    }
    for &(from, to, dep_type) in edges {
        graph.add_edge(Edge {
            from: from.into(),
            to: to.into(),
            dep_type,
        });
    }
    graph
}

/// Helper: mark a task as successfully completed.
fn complete_task(graph: &mut TaskGraph, id: &str) {
    let task = graph.get_task_mut(id).unwrap();
    task.state = RuntimeState::Terminal(TerminalState::Succeeded {
        patch_digest: format!("{id}-digest"),
        verify_run_id: format!("{id}-v1"),
        completed_at: Utc::now(),
    });
}

// ---------------------------------------------------------------------------
// Integration: DAG build -> topo sort -> work simulation -> unblocking
// ---------------------------------------------------------------------------

/// Build a realistic DAG, validate topological ordering, then simulate
/// incremental completion with unblock cascading.
#[test]
fn dag_build_topo_sort_and_cascading_unblock() {
    //   a
    //  / \
    // b   c
    //  \ /
    //   d
    //   |
    //   e
    let mut graph = build_graph(
        &["a", "b", "c", "d", "e"],
        &[
            ("a", "b", DependencyType::Blocks),
            ("a", "c", DependencyType::Blocks),
            ("b", "d", DependencyType::Blocks),
            ("c", "d", DependencyType::Blocks),
            ("d", "e", DependencyType::Blocks),
        ],
    );

    // No cycles
    assert!(!graph.has_cycles());

    // Topological sort respects order
    let sorted = graph.topological_sort().unwrap();
    assert_eq!(sorted.len(), 5);
    let pos = |id: &str| sorted.iter().position(|x| x == id).unwrap();
    assert!(pos("a") < pos("b"));
    assert!(pos("a") < pos("c"));
    assert!(pos("b") < pos("d"));
    assert!(pos("c") < pos("d"));
    assert!(pos("d") < pos("e"));

    // Roots and leaves
    let mut roots = graph.roots();
    roots.sort();
    assert_eq!(roots, vec!["a".to_string()]);
    let mut leaves = graph.leaves();
    leaves.sort();
    assert_eq!(leaves, vec!["e".to_string()]);

    // --- Simulate work ---

    // Complete 'a' -> unblocks 'b' and 'c'
    complete_task(&mut graph, "a");
    let mut unblocked = graph.newly_unblocked(&"a".into(), &Trigger::Completed);
    unblocked.sort();
    assert_eq!(unblocked, vec!["b".to_string(), "c".to_string()]);

    // Complete 'b' -> does NOT unblock 'd' (still waiting on 'c')
    complete_task(&mut graph, "b");
    let unblocked = graph.newly_unblocked(&"b".into(), &Trigger::Completed);
    assert!(
        unblocked.is_empty(),
        "d should still be blocked by c, got: {unblocked:?}",
    );

    // Complete 'c' -> NOW 'd' is unblocked
    complete_task(&mut graph, "c");
    let unblocked = graph.newly_unblocked(&"c".into(), &Trigger::Completed);
    assert_eq!(unblocked, vec!["d".to_string()]);

    // Complete 'd' -> 'e' is unblocked
    complete_task(&mut graph, "d");
    let unblocked = graph.newly_unblocked(&"d".into(), &Trigger::Completed);
    assert_eq!(unblocked, vec!["e".to_string()]);
}

/// Transitive dependency and dependent queries across a chain.
#[test]
fn transitive_dependencies_and_dependents() {
    let graph = build_graph(
        &["a", "b", "c", "d"],
        &[
            ("a", "b", DependencyType::Blocks),
            ("b", "c", DependencyType::Blocks),
            ("c", "d", DependencyType::Blocks),
        ],
    );

    // d depends on everything
    let mut deps = graph.dependencies(&"d".into());
    deps.sort();
    assert_eq!(deps, vec!["a", "b", "c"]);

    // a's dependents are everything
    let mut dependents = graph.dependents(&"a".into());
    dependents.sort();
    assert_eq!(dependents, vec!["b", "c", "d"]);

    // Direct dependencies only
    let direct = graph.direct_dependencies(&"d".into());
    assert_eq!(direct, vec!["c".to_string()]);

    let direct_dep = graph.direct_dependents(&"a".into());
    assert_eq!(direct_dep, vec!["b".to_string()]);
}

/// Cycle detection and topological sort failure.
#[test]
fn cycle_detection_and_sort_failure() {
    let graph = build_graph(
        &["a", "b", "c"],
        &[
            ("a", "b", DependencyType::Blocks),
            ("b", "c", DependencyType::Blocks),
            ("c", "a", DependencyType::Blocks),
        ],
    );

    assert!(graph.has_cycles());

    let cycles = graph.detect_cycles();
    assert!(!cycles.is_empty());

    // Topological sort should fail
    assert!(graph.topological_sort().is_err());

    // Cycle display
    let cycle_str = format!("{}", cycles[0]);
    assert!(cycle_str.contains("Cycle:"));
}

/// Parent-child edges don't create cycles.
#[test]
fn parent_child_edges_ignored_in_cycle_detection() {
    let graph = build_graph(
        &["parent", "child"],
        &[
            ("parent", "child", DependencyType::ParentChild),
            ("child", "parent", DependencyType::ParentChild),
        ],
    );

    assert!(
        !graph.has_cycles(),
        "ParentChild edges should not cause cycles"
    );
    assert!(graph.topological_sort().is_ok());
}

// ---------------------------------------------------------------------------
// Integration: Recovery — lease expiration, deferred tasks, agent registry
// ---------------------------------------------------------------------------

/// Simulate lease expiration and task recovery.
#[test]
fn lease_expiration_and_recovery() {
    let now = Utc::now();
    let now_ts = now.timestamp() as u64;

    let mut tasks: HashMap<String, Task> = HashMap::new();

    // Stale lease (expired 200 seconds ago)
    let mut stale = Task::new("stale".into(), "Stale task".into(), String::new());
    stale.state = RuntimeState::Leased {
        lease_id: "lease-1".into(),
        agent_id: "agent-dead".into(),
        fence_token: 1,
        expires_at: now - Duration::seconds(200),
    };
    stale.owner = Some("agent-dead".into());
    tasks.insert("stale".into(), stale);

    // Active lease (expires in 1 hour)
    let mut active = Task::new("active".into(), "Active task".into(), String::new());
    active.state = RuntimeState::Leased {
        lease_id: "lease-2".into(),
        agent_id: "agent-alive".into(),
        fence_token: 2,
        expires_at: now + Duration::seconds(3600),
    };
    active.owner = Some("agent-alive".into());
    tasks.insert("active".into(), active);

    // Ready task (not leased)
    tasks.insert(
        "ready".into(),
        Task::new("ready".into(), "Ready task".into(), String::new()),
    );

    // Expire with 60s grace
    let expired = expire_stale_leases(&mut tasks, 60, now_ts);

    assert_eq!(expired, vec!["stale".to_string()]);
    assert!(tasks.get("stale").unwrap().is_ready());
    assert!(tasks.get("stale").unwrap().owner.is_none());
    assert!(tasks.get("active").unwrap().is_leased());
    assert!(tasks.get("ready").unwrap().is_ready());
}

/// Deferred tasks: time-based and dependency-based resume.
#[test]
fn deferred_task_resume_workflow() {
    let now = 1000u64;

    let deferred = vec![
        DeferredTask::until_time("time-not-ready".into(), now + 500, "waiting".into()),
        DeferredTask::until_time("time-ready".into(), now - 10, "past due".into()),
        DeferredTask::until_dependency(
            "dep-not-ready".into(),
            "blocker-a".into(),
            "waiting".into(),
        ),
        DeferredTask::until_dependency("dep-ready".into(), "blocker-b".into(), "waiting".into()),
    ];

    let mut completed = HashSet::new();
    completed.insert("blocker-b");

    let ready = check_deferred_tasks(&deferred, now, &completed);
    assert_eq!(
        ready,
        vec!["time-ready".to_string(), "dep-ready".to_string()]
    );

    // After time passes and all blockers complete, everything is ready
    completed.insert("blocker-a");
    let ready = check_deferred_tasks(&deferred, now + 1000, &completed);
    assert_eq!(ready.len(), 4);
}

/// Agent registry: register, heartbeat, detect dead, unregister.
#[test]
fn agent_registry_lifecycle() {
    let mut registry = AgentRegistry::new(60);

    // Register agents
    registry.register("agent-1".into());
    registry.register("agent-2".into());
    registry.register("agent-3".into());
    assert_eq!(registry.count(), 3);

    // All idle -> all available and alive
    assert_eq!(registry.alive_agents().len(), 3);
    assert_eq!(registry.available_agents().len(), 3);

    // Agent-1 starts working
    registry.update(
        "agent-1",
        pi::task_graph::recovery::AgentState::Running {
            last_activity: 1000,
        },
    );

    // Agent-2 gets stuck
    registry.update(
        "agent-2",
        pi::task_graph::recovery::AgentState::Stuck {
            reason: "Cannot resolve dependency".into(),
        },
    );

    // Agent-2 stuck is alive but not available
    assert!(registry.get("agent-2").unwrap().is_alive());
    assert!(!registry.get("agent-2").unwrap().is_available());

    // Detect dead: agent-1 last activity was at 1000, now is 2000 (timeout 60)
    let dead = registry.detect_dead_agents(2000);
    assert_eq!(dead, vec!["agent-1".to_string()]);
    assert!(!registry.get("agent-1").unwrap().is_alive());

    // Unregister dead agent
    registry.unregister("agent-1");
    assert_eq!(registry.count(), 2);
}

/// Graph mutation: add tasks, add edges, remove task (edges cascade).
#[test]
fn graph_structural_mutation() {
    let mut graph = TaskGraph::with_capacity(5, 5);

    // Add tasks
    assert!(graph.add_task(Task::new("a".into(), "A".into(), String::new())));
    assert!(graph.add_task(Task::new("b".into(), "B".into(), String::new())));
    assert!(graph.add_task(Task::new("c".into(), "C".into(), String::new())));

    // Duplicate add fails
    assert!(!graph.add_task(Task::new("a".into(), "A2".into(), String::new())));

    // Add edges
    assert!(graph.add_edge(Edge {
        from: "a".into(),
        to: "b".into(),
        dep_type: DependencyType::Blocks,
    }));
    assert!(graph.add_edge(Edge {
        from: "b".into(),
        to: "c".into(),
        dep_type: DependencyType::Blocks,
    }));
    assert_eq!(graph.edge_count(), 2);

    // Edge to nonexistent task fails
    assert!(!graph.add_edge(Edge {
        from: "a".into(),
        to: "z".into(),
        dep_type: DependencyType::Blocks,
    }));

    // Duplicate edge fails
    assert!(!graph.add_edge(Edge {
        from: "a".into(),
        to: "b".into(),
        dep_type: DependencyType::Blocks,
    }));

    // Remove 'b' cascades edges
    let removed = graph.remove_task("b");
    assert!(removed.is_some());
    assert_eq!(removed.unwrap().subject, "B");
    assert_eq!(graph.edge_count(), 0);
    assert_eq!(graph.task_count(), 2);

    // Clear
    graph.clear();
    assert_eq!(graph.task_count(), 0);
    assert_eq!(graph.edge_count(), 0);
}
