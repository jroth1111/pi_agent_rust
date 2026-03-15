mod events;
mod store;
mod types;

pub use events::{RuntimeEvent, RuntimeEventDraft, RuntimeEventKind, RuntimeEventLog};
pub use store::{RuntimeSnapshotStore, RuntimeStore, RuntimeStoreManifest};
pub use types::{
    JobRecord, JobStatus, Metadata, ModelProfile, PlanArtifact, PolicyOutcome, PolicyVerdict,
    RunSnapshot, RunSpec, TaskNode, TaskRuntime, TaskState, JOB_RECORD_SCHEMA,
    MODEL_PROFILE_SCHEMA, PLAN_ARTIFACT_SCHEMA, POLICY_VERDICT_SCHEMA, RUN_SNAPSHOT_SCHEMA,
    RUN_SPEC_SCHEMA, TASK_NODE_SCHEMA, TASK_RUNTIME_SCHEMA,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_module_exports_are_constructible() {
        let profile = ModelProfile::new("openai", "gpt-5");
        let spec = RunSpec::new("bootstrap runtime", profile);
        let snapshot = RunSnapshot::new(spec);

        assert!(snapshot.run_id.starts_with("run-"));
        assert!(snapshot.snapshot_id.starts_with("snapshot-"));
    }
}
