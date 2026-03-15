//! Session picker listing and navigation tests (no TUI runtime).

mod common;

use asupersync::runtime::RuntimeBuilder;
use bubbletea::{KeyMsg, KeyType, Message};
use clap::Parser;
use common::TestHarness;
use pi::cli::Cli;
use pi::config::Config;
use pi::model::UserContent;
use pi::session::{Session, SessionHeader, SessionMessage, encode_cwd};
use pi::session_index::{SessionIndex, SessionMeta};
use pi::session_picker::{SessionPicker, format_time, list_sessions_for_project, pick_session};
use std::env;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::thread::sleep;
use std::time::Duration;

fn key_message(key_type: KeyType, runes: Vec<char>) -> Message {
    Message::new(KeyMsg {
        key_type,
        runes,
        alt: false,
        paste: false,
    })
}

fn write_session_file(base_dir: &Path, cwd: &Path, name: &str, id: &str) -> String {
    let project_dir = base_dir.join(encode_cwd(cwd));
    std::fs::create_dir_all(&project_dir).expect("create project session dir");
    let path = project_dir.join(name);

    let mut header = SessionHeader::new();
    header.cwd = cwd.display().to_string();
    header.id = id.to_string();
    header.timestamp = "2026-02-03T12:00:00.000Z".to_string();

    let json = serde_json::to_string(&header).expect("serialize header");
    std::fs::write(&path, format!("{json}\n")).expect("write session file");
    path.display().to_string()
}

fn selected_line(view: &str) -> Option<&str> {
    view.lines().find(|line| line.starts_with('>'))
}

fn run_async<T>(future: impl Future<Output = T>) -> T {
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .expect("runtime build");
    runtime.block_on(future)
}

fn session_picker_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(())).lock().expect("lock")
}

struct CurrentDirGuard {
    previous: PathBuf,
}

impl CurrentDirGuard {
    fn new(path: &Path) -> Self {
        let previous = env::current_dir().expect("current dir");
        env::set_current_dir(path).expect("set current dir");
        Self { previous }
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = env::set_current_dir(&self.previous);
    }
}

fn create_session(harness: &TestHarness, base_dir: &Path, cwd: &Path, label: &str) -> PathBuf {
    let mut session = Session::create_with_dir(Some(base_dir.to_path_buf()));
    session.header.cwd = cwd.display().to_string();
    session.append_message(SessionMessage::User {
        content: UserContent::Text(format!("hello {label}")),
        timestamp: Some(0),
    });
    run_async(async {
        session.save().await.expect("save session");
    });

    let path = session.path.clone().expect("session path");
    harness.log().info_ctx("setup", "Created session", |ctx| {
        ctx.push(("label".to_string(), label.to_string()));
        ctx.push(("path".to_string(), path.display().to_string()));
    });
    path
}

fn create_workflow_session(path: &Path, base_dir: &Path, cwd: &Path, id: &str) {
    let mut session = Session::create_with_dir(Some(base_dir.to_path_buf()));
    session.path = Some(path.to_path_buf());
    session.header.cwd = cwd.display().to_string();
    session.header.id = id.to_string();
    session.append_message(SessionMessage::User {
        content: UserContent::Text("workflow visibility".to_string()),
        timestamp: Some(0),
    });

    let contract = serde_json::json!({
        "schemaVersion": 1,
        "tasks": [
            {
                "id": "coord-1",
                "kind": "coordination",
                "title": "Workflow session",
                "goal": "Coordinate workflow visibility",
                "inputSnapshot": {
                    "summary": "Current session",
                    "references": [{"kind": "session_context", "locator": "session://current"}]
                },
                "priority": "medium",
                "acceptanceCriteria": [{"description": "Expose workflow visibility"}],
                "definitionOfDone": [{"description": "Workflow metadata is indexed"}],
                "contextBudget": {"maxPromptTokens": 32000, "maxOutputTokens": 4000, "maxToolCalls": 8},
                "maxAttempts": 2,
                "outputContract": {"summaryRequired": true}
            },
            {
                "id": "task-1",
                "kind": "implementation",
                "title": "Index workflow metadata",
                "goal": "Keep scan fallback workflow-aware",
                "inputSnapshot": {
                    "summary": "Current session",
                    "references": [{"kind": "session_context", "locator": "session://current"}]
                },
                "parentTaskId": "coord-1",
                "priority": "high",
                "acceptanceCriteria": [{"description": "build_meta includes workflow"}],
                "definitionOfDone": [{"description": "session picker preserves it"}],
                "contextBudget": {"maxPromptTokens": 32000, "maxOutputTokens": 4000, "maxToolCalls": 8},
                "maxAttempts": 2,
                "outputContract": {"summaryRequired": true}
            }
        ]
    });
    session
        .set_workflow_contract(serde_json::from_value(contract).expect("workflow contract"))
        .expect("set workflow contract");
    session
        .set_workflow_active_task("task-1".to_string())
        .expect("set active workflow task");

    let snapshot = serde_json::json!({
        "taskStates": [{
            "taskId": "task-1",
            "status": "in_progress",
            "attemptsUsed": 1,
            "reviewStatus": "not_requested",
            "verificationStatus": "not_requested",
            "statusSummary": "Indexing workflow visibility",
            "completionEvidence": {
                "summary": "Workflow metadata is available",
                "acceptanceCriteria": [],
                "definitionOfDone": [],
                "artifacts": []
            }
        }]
    });
    session
        .append_workflow_runtime_snapshot(
            serde_json::from_value(snapshot).expect("workflow runtime snapshot"),
        )
        .expect("append workflow runtime snapshot");

    run_async(async {
        session.save().await.expect("save workflow session");
    });
}

fn corrupt_session_header(path: &Path) {
    let raw = std::fs::read_to_string(path).expect("read session file");
    let mut lines = raw.lines().map(str::to_string).collect::<Vec<_>>();
    assert!(!lines.is_empty(), "session file should have a header line");
    lines[0] = "{ not valid json header".to_string();
    std::fs::write(path, format!("{}\n", lines.join("\n"))).expect("write corrupted session");
}

#[test]
fn format_time_parses_rfc3339() {
    let harness = TestHarness::new("format_time_parses_rfc3339");
    let input = "2026-02-03T10:30:00.000Z";
    let formatted = format_time(input);
    harness
        .log()
        .info_ctx("format", "formatted timestamp", |ctx| {
            ctx.push(("input".to_string(), input.to_string()));
            ctx.push(("output".to_string(), formatted.clone()));
        });
    assert!(formatted.contains("2026-02-03"));
    assert!(formatted.contains("10:30"));
}

#[test]
fn format_time_falls_back_for_invalid() {
    let harness = TestHarness::new("format_time_falls_back_for_invalid");
    let input = "not-a-timestamp";
    let formatted = format_time(input);
    harness
        .log()
        .info_ctx("format", "fallback timestamp", |ctx| {
            ctx.push(("input".to_string(), input.to_string()));
            ctx.push(("output".to_string(), formatted.clone()));
        });
    assert_eq!(formatted, input);
}

#[test]
fn list_sessions_for_project_returns_empty_if_missing() {
    let harness = TestHarness::new("list_sessions_for_project_returns_empty_if_missing");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");

    let sessions = list_sessions_for_project(&cwd, Some(&base_dir));
    harness.log().info_ctx("list", "sessions list", |ctx| {
        ctx.push(("count".to_string(), sessions.len().to_string()));
    });
    assert!(sessions.is_empty());
}

#[test]
fn list_sessions_for_project_orders_by_mtime() {
    let harness = TestHarness::new("list_sessions_for_project_orders_by_mtime");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");

    let first_path = write_session_file(&base_dir, &cwd, "a.jsonl", "aaaaaaa1");
    sleep(Duration::from_millis(15));
    let second_path = write_session_file(&base_dir, &cwd, "b.jsonl", "bbbbbbb2");

    let sessions = list_sessions_for_project(&cwd, Some(&base_dir));
    harness.log().info_ctx("list", "ordered sessions", |ctx| {
        ctx.push(("count".to_string(), sessions.len().to_string()));
        if let Some(first) = sessions.first() {
            ctx.push(("first".to_string(), first.path.clone()));
        }
    });
    assert!(sessions.len() >= 2);
    assert_eq!(sessions[0].path, second_path);
    assert_eq!(sessions[1].path, first_path);
}

#[test]
fn list_sessions_for_project_preserves_workflow_summary_from_disk_scan() {
    let harness =
        TestHarness::new("list_sessions_for_project_preserves_workflow_summary_from_disk_scan");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");

    let project_dir = base_dir.join(encode_cwd(&cwd));
    std::fs::create_dir_all(&project_dir).expect("create project session dir");
    let session_path = project_dir.join("workflow.jsonl");

    std::fs::write(&session_path, "stale index payload").expect("write stale payload");
    let mut stale_header = SessionHeader::new();
    stale_header.cwd = cwd.display().to_string();
    stale_header.id = "workflow-session".to_string();
    stale_header.timestamp = "2026-02-03T12:00:00.000Z".to_string();

    let index = SessionIndex::for_sessions_root(&base_dir);
    index
        .index_session_snapshot(&session_path, &stale_header, 1, None, None)
        .expect("seed stale index row without workflow metadata");

    sleep(Duration::from_millis(20));
    create_workflow_session(&session_path, &base_dir, &cwd, "workflow-session");

    let sessions = list_sessions_for_project(&cwd, Some(&base_dir));
    assert_eq!(sessions.len(), 1);

    let workflow = sessions[0]
        .workflow
        .as_ref()
        .expect("disk scan should restore workflow summary");
    harness
        .log()
        .info_ctx("workflow", "picker session metadata", |ctx| {
            ctx.push(("path".to_string(), sessions[0].path.clone()));
            ctx.push((
                "active_task_id".to_string(),
                workflow.active_task_id.clone().unwrap_or_default(),
            ));
            ctx.push((
                "snapshot_count".to_string(),
                workflow.snapshot_count.to_string(),
            ));
            ctx.push((
                "in_progress_tasks".to_string(),
                workflow.summary.in_progress_tasks.to_string(),
            ));
            ctx.push((
                "total_tasks".to_string(),
                workflow.summary.total_tasks.to_string(),
            ));
        });
    assert_eq!(workflow.active_task_id.as_deref(), Some("task-1"));
    assert_eq!(workflow.snapshot_count, 1);
    assert_eq!(workflow.summary.in_progress_tasks, 1);
    assert_eq!(workflow.summary.total_tasks, 2);
}

#[test]
fn session_picker_navigation_down_up() {
    let harness = TestHarness::new("session_picker_navigation_down_up");
    let sessions = vec![
        SessionMeta {
            path: "/tmp/a.jsonl".to_string(),
            id: "aaaaaaaa".to_string(),
            cwd: "/tmp".to_string(),
            timestamp: "2026-02-03T10:00:00.000Z".to_string(),
            message_count: 1,
            last_modified_ms: 1000,
            size_bytes: 100,
            name: None,
        },
        SessionMeta {
            path: "/tmp/b.jsonl".to_string(),
            id: "bbbbbbbb".to_string(),
            cwd: "/tmp".to_string(),
            timestamp: "2026-02-03T11:00:00.000Z".to_string(),
            message_count: 2,
            last_modified_ms: 2000,
            size_bytes: 200,
            name: None,
        },
    ];

    let mut picker = SessionPicker::new(sessions);
    let initial_view = picker.view();
    assert!(
        selected_line(&initial_view)
            .unwrap_or_default()
            .contains("aaaaaaaa")
    );

    picker.update(key_message(KeyType::Down, Vec::new()));
    let after_down = picker.view();
    harness.log().info("nav", "after Down");
    assert!(
        selected_line(&after_down)
            .unwrap_or_default()
            .contains("bbbbbbbb")
    );

    picker.update(key_message(KeyType::Up, Vec::new()));
    let after_up = picker.view();
    assert!(
        selected_line(&after_up)
            .unwrap_or_default()
            .contains("aaaaaaaa")
    );
}

#[test]
fn session_picker_navigation_with_jk() {
    let harness = TestHarness::new("session_picker_navigation_with_jk");
    let sessions = vec![
        SessionMeta {
            path: "/tmp/a.jsonl".to_string(),
            id: "aaaaaaaa".to_string(),
            cwd: "/tmp".to_string(),
            timestamp: "2026-02-03T10:00:00.000Z".to_string(),
            message_count: 1,
            last_modified_ms: 1000,
            size_bytes: 100,
            name: None,
        },
        SessionMeta {
            path: "/tmp/b.jsonl".to_string(),
            id: "bbbbbbbb".to_string(),
            cwd: "/tmp".to_string(),
            timestamp: "2026-02-03T11:00:00.000Z".to_string(),
            message_count: 2,
            last_modified_ms: 2000,
            size_bytes: 200,
            name: None,
        },
    ];

    let mut picker = SessionPicker::new(sessions);
    picker.update(key_message(KeyType::Runes, vec!['j']));
    let after_j = picker.view();
    harness.log().info("nav", "after j");
    assert!(
        selected_line(&after_j)
            .unwrap_or_default()
            .contains("bbbbbbbb")
    );

    picker.update(key_message(KeyType::Runes, vec!['k']));
    let after_k = picker.view();
    assert!(
        selected_line(&after_k)
            .unwrap_or_default()
            .contains("aaaaaaaa")
    );
}

#[test]
fn session_picker_enter_sets_chosen_path() {
    let harness = TestHarness::new("session_picker_enter_sets_chosen_path");
    let sessions = vec![SessionMeta {
        path: "/tmp/a.jsonl".to_string(),
        id: "aaaaaaaa".to_string(),
        cwd: "/tmp".to_string(),
        timestamp: "2026-02-03T10:00:00.000Z".to_string(),
        message_count: 1,
        last_modified_ms: 1000,
        size_bytes: 100,
        name: None,
    }];

    let mut picker = SessionPicker::new(sessions);
    picker.update(key_message(KeyType::Enter, Vec::new()));
    harness.log().info("select", "pressed Enter");
    assert_eq!(picker.selected_path(), Some("/tmp/a.jsonl"));
}

#[test]
fn session_picker_cancel_sets_flag() {
    let harness = TestHarness::new("session_picker_cancel_sets_flag");
    let sessions = vec![SessionMeta {
        path: "/tmp/a.jsonl".to_string(),
        id: "aaaaaaaa".to_string(),
        cwd: "/tmp".to_string(),
        timestamp: "2026-02-03T10:00:00.000Z".to_string(),
        message_count: 1,
        last_modified_ms: 1000,
        size_bytes: 100,
        name: None,
    }];

    let mut picker = SessionPicker::new(sessions);
    picker.update(key_message(KeyType::Esc, Vec::new()));
    harness.log().info("cancel", "pressed Esc");
    assert!(picker.was_cancelled());
}

#[test]
fn pick_session_returns_none_when_no_sessions() {
    let _lock = session_picker_lock();
    let harness = TestHarness::new("pick_session_returns_none_when_no_sessions");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let _guard = CurrentDirGuard::new(&cwd);

    let result = run_async(pick_session(Some(&base_dir)));
    harness
        .log()
        .info_ctx("result", "pick_session returned", |ctx| {
            ctx.push(("is_some".to_string(), result.is_some().to_string()));
        });
    assert!(result.is_none());
}

#[test]
fn pick_session_returns_session_when_single_entry() {
    let _lock = session_picker_lock();
    let harness = TestHarness::new("pick_session_returns_session_when_single_entry");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let _guard = CurrentDirGuard::new(&cwd);

    let session_path = create_session(&harness, &base_dir, &cwd, "single");
    let session = run_async(pick_session(Some(&base_dir))).expect("pick session");
    harness
        .log()
        .info_ctx("verify", "picked session path", |ctx| {
            ctx.push(("expected".to_string(), session_path.display().to_string()));
            ctx.push((
                "actual".to_string(),
                session
                    .path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            ));
        });
    assert_eq!(session.path.as_ref(), Some(&session_path));
    assert_eq!(session.session_dir.as_ref(), Some(&base_dir));
}

#[test]
fn resume_with_picker_creates_new_session_when_project_dir_missing() {
    let _lock = session_picker_lock();
    let harness =
        TestHarness::new("resume_with_picker_creates_new_session_when_project_dir_missing");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let _guard = CurrentDirGuard::new(&cwd);

    let session = run_async(async {
        Box::pin(Session::resume_with_picker(
            Some(&base_dir),
            &Config::default(),
            None,
        ))
        .await
        .expect("resume")
    });
    harness.log().info_ctx("verify", "resume result", |ctx| {
        ctx.push((
            "path".to_string(),
            session
                .path
                .as_ref()
                .map_or_else(|| "<none>".to_string(), |p| p.display().to_string()),
        ));
    });
    assert!(session.path.is_none());
    assert_eq!(session.session_dir.as_ref(), Some(&base_dir));
}

#[test]
fn resume_with_picker_creates_new_session_when_sessions_empty() {
    let _lock = session_picker_lock();
    let harness = TestHarness::new("resume_with_picker_creates_new_session_when_sessions_empty");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let _guard = CurrentDirGuard::new(&cwd);
    let project_dir = base_dir.join(encode_cwd(&cwd));
    std::fs::create_dir_all(&project_dir).expect("create project dir");

    let session = run_async(async {
        Box::pin(Session::resume_with_picker(
            Some(&base_dir),
            &Config::default(),
            None,
        ))
        .await
        .expect("resume")
    });
    harness.log().info_ctx("verify", "resume result", |ctx| {
        ctx.push((
            "path".to_string(),
            session
                .path
                .as_ref()
                .map_or_else(|| "<none>".to_string(), |p| p.display().to_string()),
        ));
        ctx.push((
            "session_dir".to_string(),
            session
                .session_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        ));
    });
    assert!(session.path.is_none());
    assert_eq!(session.session_dir.as_ref(), Some(&base_dir));
}

#[test]
fn resume_with_picker_selects_session_with_override_input() {
    let _lock = session_picker_lock();
    let harness = TestHarness::new("resume_with_picker_selects_session_with_override_input");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let _guard = CurrentDirGuard::new(&cwd);

    let first_path = create_session(&harness, &base_dir, &cwd, "first");
    sleep(Duration::from_millis(20));
    let second_path = create_session(&harness, &base_dir, &cwd, "second");

    let session = run_async(async {
        Box::pin(Session::resume_with_picker(
            Some(&base_dir),
            &Config::default(),
            Some("2".to_string()),
        ))
        .await
        .expect("resume")
    });

    harness.log().info_ctx("verify", "selected session", |ctx| {
        ctx.push(("expected".to_string(), first_path.display().to_string()));
        ctx.push(("newest".to_string(), second_path.display().to_string()));
        ctx.push((
            "actual".to_string(),
            session
                .path
                .as_ref()
                .map_or_else(|| "<none>".to_string(), |p| p.display().to_string()),
        ));
    });

    assert_eq!(session.path.as_ref(), Some(&first_path));
    assert_eq!(session.session_dir.as_ref(), Some(&base_dir));
}

#[test]
fn session_new_resume_uses_config_session_picker_input() {
    let _lock = session_picker_lock();
    let harness = TestHarness::new("session_new_resume_uses_config_session_picker_input");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let _guard = CurrentDirGuard::new(&cwd);

    let first_path = create_session(&harness, &base_dir, &cwd, "first");
    sleep(Duration::from_millis(20));
    let second_path = create_session(&harness, &base_dir, &cwd, "second");

    let cli = Cli::parse_from([
        "pi".to_string(),
        "--resume".to_string(),
        "--session-dir".to_string(),
        base_dir.display().to_string(),
    ]);
    let config = Config {
        session_picker_input: Some(2),
        ..Config::default()
    };

    let session = run_async(Session::new(&cli, &config)).expect("resume with config selection");

    harness
        .log()
        .info_ctx("verify", "selected via config", |ctx| {
            ctx.push(("expected".to_string(), first_path.display().to_string()));
            ctx.push(("newest".to_string(), second_path.display().to_string()));
            ctx.push((
                "actual".to_string(),
                session
                    .path
                    .as_ref()
                    .map_or_else(|| "<none>".to_string(), |p| p.display().to_string()),
            ));
        });

    assert_eq!(session.path.as_ref(), Some(&first_path));
    assert_eq!(session.session_dir.as_ref(), Some(&base_dir));
}

#[test]
fn continue_recent_skips_unreadable_newest_indexed_session() {
    let _lock = session_picker_lock();
    let harness = TestHarness::new("continue_recent_skips_unreadable_newest_indexed_session");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let _guard = CurrentDirGuard::new(&cwd);

    let older_path = create_session(&harness, &base_dir, &cwd, "older");
    sleep(Duration::from_millis(20));
    let newest_path = create_session(&harness, &base_dir, &cwd, "newest");
    corrupt_session_header(&newest_path);
    harness.record_artifact("corrupted-newest.jsonl", &newest_path);

    let resumed = run_async(async {
        Session::continue_recent_in_dir(Some(&base_dir), &Config::default())
            .await
            .expect("continue recent")
    });

    assert_eq!(resumed.path.as_ref(), Some(&older_path));
    assert_eq!(resumed.session_dir.as_ref(), Some(&base_dir));
}

#[test]
fn resume_with_picker_selected_unreadable_session_falls_back_to_new() {
    let _lock = session_picker_lock();
    let harness =
        TestHarness::new("resume_with_picker_selected_unreadable_session_falls_back_to_new");
    let base_dir = harness.temp_path("sessions");
    let cwd = harness.temp_path("project");
    std::fs::create_dir_all(&cwd).expect("create cwd");
    let _guard = CurrentDirGuard::new(&cwd);

    let broken_path = create_session(&harness, &base_dir, &cwd, "broken");
    let old_id = run_async(async {
        Session::open(broken_path.to_string_lossy().as_ref())
            .await
            .expect("open created session")
            .header
            .id
    });
    corrupt_session_header(&broken_path);
    harness.record_artifact("corrupted-selected.jsonl", &broken_path);

    let resumed = run_async(async {
        Box::pin(Session::resume_with_picker(
            Some(&base_dir),
            &Config::default(),
            Some("1".to_string()),
        ))
        .await
        .expect("resume with picker")
    });

    assert!(resumed.path.is_none(), "expected fresh unsaved session");
    assert_eq!(resumed.session_dir.as_ref(), Some(&base_dir));
    assert_ne!(resumed.header.id, old_id);
}
