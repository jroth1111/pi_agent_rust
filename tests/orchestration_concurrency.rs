//! Loom-based concurrency tests for orchestration module.
//!
//! Tests focus on coordination patterns for multi-agent flock management:
//! - Worker assignment tracking
//! - File ownership tracking (conflict detection)
//! - Merge state machine coordination

#[cfg(test)]
mod loom_tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    /// Model of file ownership tracking for flock workers.
    ///
    /// Tracks which worker owns which files to detect conflicts.
    /// Uses `RwLock` to allow concurrent conflict detection while
    /// serializing ownership updates.
    struct FileOwnership {
        /// Maps file path -> worker `segment_id`
        ownership: loom::sync::RwLock<HashMap<String, usize>>,
    }

    impl FileOwnership {
        fn new() -> Self {
            Self {
                ownership: loom::sync::RwLock::new(HashMap::new()),
            }
        }

        /// Try to claim ownership of a file for a worker.
        /// Returns true if claimed, false if already owned by another.
        fn claim(&self, file: &str, worker_id: usize) -> bool {
            let mut guard = self.ownership.write().unwrap();
            match guard.get(file) {
                Some(&owner) if owner != worker_id => false,
                _ => {
                    guard.insert(file.to_string(), worker_id);
                    true
                }
            }
        }

        /// Release ownership of a file.
        fn release(&self, file: &str, worker_id: usize) {
            let mut guard = self.ownership.write().unwrap();
            if let Some(&owner) = guard.get(file) {
                if owner == worker_id {
                    guard.remove(file);
                }
            }
        }

        /// Check if a file is owned by a specific worker.
        fn is_owned_by(&self, file: &str, worker_id: usize) -> bool {
            let guard = self.ownership.read().unwrap();
            guard.get(file) == Some(&worker_id)
        }

        /// Check if a file is owned by any worker.
        fn is_owned(&self, file: &str) -> bool {
            let guard = self.ownership.read().unwrap();
            guard.contains_key(file)
        }

        /// Get the owner of a file.
        fn get_owner(&self, file: &str) -> Option<usize> {
            let guard = self.ownership.read().unwrap();
            guard.get(file).copied()
        }
    }

    /// Test: File claim exclusion.
    ///
    /// Two workers trying to claim the same file - only one should succeed.
    #[test]
    fn file_claim_exclusion() {
        loom::model(|| {
            let ownership = Arc::new(FileOwnership::new());

            let o1 = Arc::clone(&ownership);
            let o2 = Arc::clone(&ownership);

            let th1 = loom::thread::spawn(move || o1.claim("src/main.rs", 0));

            let th2 = loom::thread::spawn(move || o2.claim("src/main.rs", 1));

            let r1 = th1.join().unwrap();
            let r2 = th2.join().unwrap();

            // Exactly one should succeed
            assert_ne!(r1, r2);

            // Verify final owner
            let owner = ownership.get_owner("src/main.rs");
            if r1 {
                assert_eq!(owner, Some(0));
            } else {
                assert_eq!(owner, Some(1));
            }
        });
    }

    /// Test: Re-claim by same owner.
    ///
    /// A worker should be able to re-claim a file they already own.
    #[test]
    fn file_reclaim_same_owner() {
        loom::model(|| {
            let ownership = Arc::new(FileOwnership::new());

            // Worker 0 claims
            assert!(ownership.claim("file.rs", 0));

            let o1 = Arc::clone(&ownership);
            let th1 = loom::thread::spawn(move || {
                o1.claim("file.rs", 0) // Same owner re-claims
            });

            let result = th1.join().unwrap();
            assert!(result); // Should succeed
        });
    }

    /// Test: Release and re-claim by different owner.
    ///
    /// After releasing, another worker should be able to claim.
    #[test]
    fn file_release_and_reclaim() {
        loom::model(|| {
            let ownership = Arc::new(FileOwnership::new());

            ownership.claim("file.rs", 0);
            ownership.release("file.rs", 0);

            let o1 = Arc::clone(&ownership);
            let th1 = loom::thread::spawn(move || o1.claim("file.rs", 1));

            let result = th1.join().unwrap();
            assert!(result);
            assert_eq!(ownership.get_owner("file.rs"), Some(1));
        });
    }

    /// Test: Concurrent reads of ownership state.
    ///
    /// Multiple threads checking ownership should not conflict.
    #[test]
    fn concurrent_ownership_reads() {
        loom::model(|| {
            let ownership = Arc::new(FileOwnership::new());
            ownership.claim("file.rs", 0);

            let o1 = Arc::clone(&ownership);
            let o2 = Arc::clone(&ownership);
            let o3 = Arc::clone(&ownership);

            let th1 = loom::thread::spawn(move || o1.is_owned("file.rs"));

            let th2 = loom::thread::spawn(move || o2.is_owned_by("file.rs", 0));

            let th3 = loom::thread::spawn(move || o3.get_owner("file.rs"));

            assert!(th1.join().unwrap());
            assert!(th2.join().unwrap());
            assert_eq!(th3.join().unwrap(), Some(0));
        });
    }

    /// Model of merge state machine.
    ///
    /// Tracks the state of a merge operation across multiple workers.
    /// Uses atomic state transitions for correctness.
    mod merge_state {
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum State {
            Pending,
            InProgress,
            Completed,
            Failed,
        }

        impl State {
            fn as_usize(self) -> usize {
                match self {
                    Self::Pending => 0,
                    Self::InProgress => 1,
                    Self::Completed => 2,
                    Self::Failed => 3,
                }
            }

            fn from_usize(v: usize) -> Self {
                match v {
                    0 => Self::Pending,
                    1 => Self::InProgress,
                    2 => Self::Completed,
                    3 => Self::Failed,
                    _ => Self::Failed,
                }
            }
        }

        pub struct MergeStateMachine {
            state: AtomicUsize,
        }

        impl MergeStateMachine {
            pub fn new() -> Self {
                Self {
                    state: AtomicUsize::new(State::Pending.as_usize()),
                }
            }

            /// Try to transition to `InProgress`.
            /// Returns true if transition succeeded.
            pub fn start(&self) -> bool {
                self.state
                    .compare_exchange(
                        State::Pending.as_usize(),
                        State::InProgress.as_usize(),
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_ok()
            }

            /// Complete the merge.
            pub fn complete(&self) -> bool {
                self.state
                    .compare_exchange(
                        State::InProgress.as_usize(),
                        State::Completed.as_usize(),
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_ok()
            }

            /// Fail the merge.
            pub fn fail(&self) -> bool {
                let current = self.state.load(Ordering::SeqCst);
                if current == State::Completed.as_usize() {
                    return false; // Can't fail after completion
                }
                self.state
                    .compare_exchange(
                        current,
                        State::Failed.as_usize(),
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    )
                    .is_ok()
            }

            pub fn state(&self) -> State {
                State::from_usize(self.state.load(Ordering::SeqCst))
            }
        }
    }

    use merge_state::{MergeStateMachine, State};

    /// Test: Only one thread can start merge.
    #[test]
    fn merge_start_exclusion() {
        loom::model(|| {
            let machine = Arc::new(MergeStateMachine::new());

            let m1 = Arc::clone(&machine);
            let m2 = Arc::clone(&machine);

            let th1 = loom::thread::spawn(move || m1.start());

            let th2 = loom::thread::spawn(move || m2.start());

            let r1 = th1.join().unwrap();
            let r2 = th2.join().unwrap();

            // Exactly one should succeed
            assert_ne!(r1, r2);

            // State should be InProgress
            assert_eq!(machine.state(), State::InProgress);
        });
    }

    /// Test: Complete after start.
    #[test]
    fn merge_complete_after_start() {
        loom::model(|| {
            let machine = Arc::new(MergeStateMachine::new());
            machine.start();

            let m1 = Arc::clone(&machine);
            let th1 = loom::thread::spawn(move || m1.complete());

            assert!(th1.join().unwrap());
            assert_eq!(machine.state(), State::Completed);
        });
    }

    /// Test: Cannot start after completion.
    #[test]
    fn merge_no_start_after_complete() {
        loom::model(|| {
            let machine = Arc::new(MergeStateMachine::new());
            machine.start();
            machine.complete();

            assert!(!machine.start());
            assert_eq!(machine.state(), State::Completed);
        });
    }

    /// Test: Fail transitions.
    #[test]
    fn merge_fail_transition() {
        loom::model(|| {
            let machine = Arc::new(MergeStateMachine::new());

            let m1 = Arc::clone(&machine);
            let m2 = Arc::clone(&machine);

            let th1 = loom::thread::spawn(move || m1.start());

            let th2 = loom::thread::spawn(move || m2.fail());

            let started = th1.join().unwrap();
            let failed = th2.join().unwrap();

            // If start succeeded, fail might or might not succeed depending on timing
            // If fail succeeded, start should have failed
            if started {
                // State is InProgress or Failed (if fail came after)
                let state = machine.state();
                assert!(state == State::InProgress || state == State::Failed);
            } else {
                // Start failed, so fail must have run first
                assert!(failed || machine.state() == State::Pending);
            }
        });
    }

    /// Model of worker completion tracking.
    ///
    /// Tracks how many workers have completed their work.
    /// Uses atomic counter with a barrier for coordination.
    struct WorkerCompletion {
        completed_count: loom::sync::atomic::AtomicUsize,
        total_workers: usize,
        all_complete: loom::sync::Condvar,
        lock: loom::sync::Mutex<()>,
    }

    impl WorkerCompletion {
        fn new(total: usize) -> Self {
            Self {
                completed_count: loom::sync::atomic::AtomicUsize::new(0),
                total_workers: total,
                all_complete: loom::sync::Condvar::new(),
                lock: loom::sync::Mutex::new(()),
            }
        }

        fn mark_complete(&self) {
            let count = self
                .completed_count
                .fetch_add(1, loom::sync::atomic::Ordering::SeqCst);
            if count + 1 == self.total_workers {
                // Last worker to complete - notify waiters
                let _guard = self.lock.lock().unwrap();
                self.all_complete.notify_all();
            }
        }

        #[allow(dead_code)]
        fn wait_for_all(&self) {
            let mut guard = self.lock.lock().unwrap();
            while self
                .completed_count
                .load(loom::sync::atomic::Ordering::SeqCst)
                < self.total_workers
            {
                guard = self.all_complete.wait(guard).unwrap();
            }
        }

        fn completed(&self) -> usize {
            self.completed_count
                .load(loom::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Test: Worker completion tracking.
    #[test]
    fn worker_completion_tracking() {
        loom::model(|| {
            let completion = Arc::new(WorkerCompletion::new(2));

            let c1 = Arc::clone(&completion);
            let c2 = Arc::clone(&completion);

            let th1 = loom::thread::spawn(move || {
                c1.mark_complete();
            });

            let th2 = loom::thread::spawn(move || {
                c2.mark_complete();
            });

            th1.join().unwrap();
            th2.join().unwrap();

            assert_eq!(completion.completed(), 2);
        });
    }

    /// Model of conflict detection.
    ///
    /// Uses a concurrent map to track which files are being modified.
    struct ConflictDetector {
        modifications: loom::sync::RwLock<HashMap<String, Vec<usize>>>,
    }

    impl ConflictDetector {
        fn new() -> Self {
            Self {
                modifications: loom::sync::RwLock::new(HashMap::new()),
            }
        }

        fn record_modification(&self, file: &str, worker_id: usize) {
            let mut guard = self.modifications.write().unwrap();
            guard.entry(file.to_string()).or_default().push(worker_id);
        }

        fn has_conflict(&self, file: &str) -> bool {
            let guard = self.modifications.read().unwrap();
            guard.get(file).is_some_and(|v| v.len() > 1)
        }

        fn get_conflicting_workers(&self, file: &str) -> Vec<usize> {
            let guard = self.modifications.read().unwrap();
            guard.get(file).cloned().unwrap_or_default()
        }
    }

    /// Test: Conflict detection.
    #[test]
    fn conflict_detection() {
        loom::model(|| {
            let detector = Arc::new(ConflictDetector::new());

            let d1 = Arc::clone(&detector);
            let d2 = Arc::clone(&detector);

            let th1 = loom::thread::spawn(move || {
                d1.record_modification("file.rs", 0);
            });

            let th2 = loom::thread::spawn(move || {
                d2.record_modification("file.rs", 1);
            });

            th1.join().unwrap();
            th2.join().unwrap();

            // Both workers modified the same file - conflict!
            assert!(detector.has_conflict("file.rs"));

            let workers = detector.get_conflicting_workers("file.rs");
            assert_eq!(workers.len(), 2);
            assert!(workers.contains(&0));
            assert!(workers.contains(&1));
        });
    }

    /// Test: No conflict for different files.
    #[test]
    fn no_conflict_different_files() {
        loom::model(|| {
            let detector = Arc::new(ConflictDetector::new());

            let d1 = Arc::clone(&detector);
            let d2 = Arc::clone(&detector);

            let th1 = loom::thread::spawn(move || {
                d1.record_modification("file1.rs", 0);
            });

            let th2 = loom::thread::spawn(move || {
                d2.record_modification("file2.rs", 1);
            });

            th1.join().unwrap();
            th2.join().unwrap();

            assert!(!detector.has_conflict("file1.rs"));
            assert!(!detector.has_conflict("file2.rs"));
        });
    }

    /// Model of segment assignment.
    ///
    /// Assigns files to worker segments using atomic operations.
    struct SegmentAssigner {
        next_segment: loom::sync::atomic::AtomicUsize,
    }

    impl SegmentAssigner {
        fn new() -> Self {
            Self {
                next_segment: loom::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn assign_segment(&self) -> usize {
            self.next_segment
                .fetch_add(1, loom::sync::atomic::Ordering::SeqCst)
        }

        fn segment_count(&self) -> usize {
            self.next_segment.load(loom::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Test: Segment assignment is unique.
    #[test]
    fn segment_assignment_unique() {
        loom::model(|| {
            let assigner = Arc::new(SegmentAssigner::new());

            let a1 = Arc::clone(&assigner);
            let a2 = Arc::clone(&assigner);

            let th1 = loom::thread::spawn(move || a1.assign_segment());

            let th2 = loom::thread::spawn(move || a2.assign_segment());

            let s1 = th1.join().unwrap();
            let s2 = th2.join().unwrap();

            // Segments must be unique
            assert_ne!(s1, s2);
            assert_eq!(assigner.segment_count(), 2);
        });
    }

    /// Test: Combined flock workflow.
    ///
    /// Simulates: segment assignment -> file claim -> modification -> conflict check
    #[test]
    fn flock_combined_workflow() {
        loom::model(|| {
            let assigner = Arc::new(SegmentAssigner::new());
            let ownership = Arc::new(FileOwnership::new());
            let detector = Arc::new(ConflictDetector::new());

            // Thread 1: Worker 0
            let assigner1 = Arc::clone(&assigner);
            let ownership1 = Arc::clone(&ownership);
            let detector1 = Arc::clone(&detector);

            let th1 = loom::thread::spawn(move || {
                let segment = assigner1.assign_segment();
                if ownership1.claim("file.rs", segment) {
                    detector1.record_modification("file.rs", segment);
                    true
                } else {
                    false
                }
            });

            // Thread 2: Worker 1
            let assigner2 = Arc::clone(&assigner);
            let ownership2 = Arc::clone(&ownership);
            let detector2 = Arc::clone(&detector);

            let th2 = loom::thread::spawn(move || {
                let segment = assigner2.assign_segment();
                if ownership2.claim("file.rs", segment) {
                    detector2.record_modification("file.rs", segment);
                    true
                } else {
                    false
                }
            });

            let claimed1 = th1.join().unwrap();
            let claimed2 = th2.join().unwrap();

            // Only one should claim, so no conflict in detector
            assert_ne!(claimed1, claimed2);
            assert!(!detector.has_conflict("file.rs"));
        });
    }
}

#[cfg(test)]
mod standard_concurrency_tests {
    //! Standard concurrency tests for orchestration patterns.

    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::RwLock;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::thread;

    /// Test real file ownership tracking.
    #[test]
    fn real_file_ownership() {
        let ownership = Arc::new(RwLock::new(HashMap::<String, usize>::new()));

        let mut handles = vec![];

        for worker_id in 0..10 {
            let ownership = Arc::clone(&ownership);
            handles.push(thread::spawn(move || {
                let mut claimed = 0;
                for i in 0..10 {
                    let file = format!("file-{}.rs", worker_id * 10 + i);
                    {
                        let mut guard = ownership.write().unwrap();
                        if let std::collections::hash_map::Entry::Vacant(e) = guard.entry(file) {
                            e.insert(worker_id);
                            claimed += 1;
                        }
                    }
                }
                claimed
            }));
        }

        let total_claimed: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();

        // All 100 files should be claimed
        assert_eq!(total_claimed, 100);

        let guard = ownership.read().unwrap();
        assert_eq!(guard.len(), 100);
    }

    /// Test real conflict detection with concurrent modifications.
    #[test]
    fn real_conflict_detection() {
        let modifications = Arc::new(RwLock::new(HashMap::<String, Vec<usize>>::new()));

        let mut handles = vec![];

        // Each worker modifies a subset of files
        for worker_id in 0..5 {
            let modifications = Arc::clone(&modifications);
            handles.push(thread::spawn(move || {
                for file_id in 0..10 {
                    let file = format!("file-{file_id}.rs");
                    {
                        let mut guard = modifications.write().unwrap();
                        guard.entry(file.clone()).or_default().push(worker_id);
                    }
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Check for conflicts
        let guard = modifications.read().unwrap();
        let conflicts: Vec<_> = guard
            .iter()
            .filter(|(_, workers)| workers.len() > 1)
            .collect();

        // Files 0-9 were modified by all 5 workers
        assert_eq!(conflicts.len(), 10);
    }

    /// Test atomic segment assignment.
    #[test]
    fn real_segment_assignment() {
        let next_segment = Arc::new(AtomicUsize::new(0));

        let mut handles = vec![];

        for _ in 0..10 {
            let next_segment = Arc::clone(&next_segment);
            handles.push(thread::spawn(move || {
                let mut segments = vec![];
                for _ in 0..100 {
                    let segment = next_segment.fetch_add(1, Ordering::SeqCst);
                    segments.push(segment);
                }
                segments
            }));
        }

        let all_segments: Vec<usize> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();

        // All 1000 segments should be unique
        let mut unique = all_segments;
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 1000);
    }

    /// Test merge state machine with real threads.
    #[test]
    fn real_merge_state_machine() {
        use std::sync::atomic::AtomicUsize;

        const PENDING: usize = 0;
        const IN_PROGRESS: usize = 1;
        const COMPLETED: usize = 2;
        const _FAILED: usize = 3;

        let state = Arc::new(AtomicUsize::new(PENDING));

        let mut handles = vec![];

        // Try to start from multiple threads
        for _ in 0..10 {
            let state = Arc::clone(&state);
            handles.push(thread::spawn(move || {
                state
                    .compare_exchange(PENDING, IN_PROGRESS, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            }));
        }

        let results: Vec<bool> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // Exactly one should succeed
        assert_eq!(results.iter().filter(|&&r| r).count(), 1);
        assert_eq!(state.load(Ordering::SeqCst), IN_PROGRESS);

        // Complete
        assert!(
            state
                .compare_exchange(IN_PROGRESS, COMPLETED, Ordering::SeqCst, Ordering::SeqCst,)
                .is_ok()
        );

        assert_eq!(state.load(Ordering::SeqCst), COMPLETED);
    }

    /// Test worker completion barrier.
    #[test]
    fn real_worker_completion_barrier() {
        use std::sync::{Condvar, Mutex};

        let completed = Arc::new(AtomicUsize::new(0));
        let pair = Arc::new((Mutex::new(()), Condvar::new()));

        let total_workers = 10;
        let mut handles = vec![];

        for _ in 0..total_workers {
            let completed = Arc::clone(&completed);
            let pair = Arc::clone(&pair);
            handles.push(thread::spawn(move || {
                // Simulate work
                thread::sleep(std::time::Duration::from_millis(1));

                let count = completed.fetch_add(1, Ordering::SeqCst);
                if count + 1 == total_workers {
                    // Last worker notifies
                    let (lock, cvar) = &*pair;
                    let _guard = lock.lock().unwrap();
                    cvar.notify_all();
                }
            }));
        }

        // Wait for all workers
        let (lock, cvar) = &*pair;
        let mut guard = lock.lock().unwrap();
        while completed.load(Ordering::SeqCst) < total_workers {
            guard = cvar.wait(guard).unwrap();
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(completed.load(Ordering::SeqCst), total_workers);
    }

    /// Test high-contention file claim scenario.
    #[test]
    fn real_high_contention_claim() {
        let ownership = Arc::new(RwLock::new(HashMap::<String, usize>::new()));
        let claim_counts = Arc::new(AtomicUsize::new(0));

        let mut handles = vec![];

        // Many workers trying to claim the same file
        for worker_id in 0..20 {
            let ownership = Arc::clone(&ownership);
            let claim_counts = Arc::clone(&claim_counts);
            handles.push(thread::spawn(move || {
                let mut guard = ownership.write().unwrap();
                if !guard.contains_key("contested.rs") {
                    guard.insert("contested.rs".to_string(), worker_id);
                    claim_counts.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Exactly one should succeed
        assert_eq!(claim_counts.load(Ordering::SeqCst), 1);

        let guard = ownership.read().unwrap();
        assert!(guard.contains_key("contested.rs"));
    }

    /// Test atomic counter for entry sequencing.
    #[test]
    fn real_atomic_entry_sequence() {
        let entry_seq = Arc::new(AtomicU64::new(1));

        let mut handles = vec![];

        for _ in 0..10 {
            let entry_seq = Arc::clone(&entry_seq);
            handles.push(thread::spawn(move || {
                let mut allocated = vec![];
                for _ in 0..100 {
                    let seq = entry_seq.fetch_add(1, Ordering::SeqCst);
                    allocated.push(seq);
                }
                allocated
            }));
        }

        let all_allocated: Vec<u64> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();

        // All should be unique
        let mut unique = all_allocated;
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 1000);

        // Final value should be 1001
        assert_eq!(entry_seq.load(Ordering::SeqCst), 1001);
    }
}
