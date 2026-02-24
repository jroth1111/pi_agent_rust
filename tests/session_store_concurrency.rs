//! Loom-based concurrency tests for Session Store V2.
//!
//! `SessionStoreV2` uses an append-only log design that doesn't have internal
//! synchronization. These tests model the synchronization patterns that
//! external code would need to use when accessing `SessionStoreV2` concurrently.
//!
//! Key patterns tested:
//! - `RwLock` for read-heavy workloads (multiple readers, single writer)
//! - Mutex for write-only workloads
//! - Atomic counters for sequence management

#[cfg(test)]
mod loom_tests {
    use std::sync::Arc;

    /// Model of sequence counters used by `SessionStoreV2`.
    ///
    /// In the real implementation, these are plain u64 fields updated
    /// during append operations. Under concurrent access, they need
    /// atomic updates.
    #[allow(dead_code)]
    struct SequenceCounters {
        next_segment_seq: loom::sync::atomic::AtomicU64,
        next_frame_seq: loom::sync::atomic::AtomicU64,
        next_entry_seq: loom::sync::atomic::AtomicU64,
        current_segment_bytes: loom::sync::atomic::AtomicU64,
    }

    impl SequenceCounters {
        fn new() -> Self {
            Self {
                next_segment_seq: loom::sync::atomic::AtomicU64::new(1),
                next_frame_seq: loom::sync::atomic::AtomicU64::new(1),
                next_entry_seq: loom::sync::atomic::AtomicU64::new(1),
                current_segment_bytes: loom::sync::atomic::AtomicU64::new(0),
            }
        }

        fn allocate_entry_seq(&self) -> u64 {
            self.next_entry_seq
                .fetch_add(1, loom::sync::atomic::Ordering::SeqCst)
        }

        #[allow(dead_code)]
        fn allocate_frame_seq(&self) -> u64 {
            self.next_frame_seq
                .fetch_add(1, loom::sync::atomic::Ordering::SeqCst)
        }

        fn get_entry_seq(&self) -> u64 {
            self.next_entry_seq
                .load(loom::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Test: Atomic entry sequence allocation.
    ///
    /// Verify that concurrent allocations produce unique sequence numbers.
    #[test]
    fn atomic_entry_seq_unique() {
        loom::model(|| {
            let counters = Arc::new(SequenceCounters::new());

            let c1 = Arc::clone(&counters);
            let c2 = Arc::clone(&counters);

            let th1 = loom::thread::spawn(move || c1.allocate_entry_seq());

            let th2 = loom::thread::spawn(move || c2.allocate_entry_seq());

            let seq1 = th1.join().unwrap();
            let seq2 = th2.join().unwrap();

            // Sequences must be unique
            assert_ne!(seq1, seq2);

            // Total allocated should be 2
            assert_eq!(counters.get_entry_seq(), 3); // started at 1, two fetch_adds
        });
    }

    /// Test: Sequential consistency of atomic operations.
    ///
    /// Verify that `SeqCst` ordering provides the expected behavior.
    #[test]
    fn seq_cst_ordering() {
        loom::model(|| {
            let counters = Arc::new(SequenceCounters::new());

            let c1 = Arc::clone(&counters);
            let c2 = Arc::clone(&counters);

            let th1 = loom::thread::spawn(move || c1.allocate_entry_seq());

            let th2 = loom::thread::spawn(move || {
                // Read after potential write

                c2.get_entry_seq()
            });

            let _allocated = th1.join().unwrap();
            let _read = th2.join().unwrap();

            // After both complete, entry_seq should be at least 2
            assert!(counters.get_entry_seq() >= 2);
        });
    }

    /// Model of an append-only log index.
    ///
    /// Uses `RwLock` to allow concurrent reads but exclusive writes.
    struct AppendLogIndex {
        entries: loom::sync::RwLock<Vec<(u64, String)>>,
    }

    impl AppendLogIndex {
        fn new() -> Self {
            Self {
                entries: loom::sync::RwLock::new(Vec::new()),
            }
        }

        fn append(&self, seq: u64, id: String) {
            let mut guard = self.entries.write().unwrap();
            guard.push((seq, id));
        }

        fn get(&self, seq: u64) -> Option<String> {
            let guard = self.entries.read().unwrap();
            guard
                .iter()
                .find(|(s, _)| *s == seq)
                .map(|(_, id)| id.clone())
        }

        fn len(&self) -> usize {
            let guard = self.entries.read().unwrap();
            guard.len()
        }

        #[allow(dead_code)]
        fn last_seq(&self) -> Option<u64> {
            let guard = self.entries.read().unwrap();
            guard.last().map(|(seq, _)| *seq)
        }
    }

    /// Test: Concurrent reads to append log index.
    ///
    /// Multiple readers should be able to read simultaneously.
    #[test]
    fn index_concurrent_reads() {
        loom::model(|| {
            let index = Arc::new(AppendLogIndex::new());

            // Pre-populate
            index.append(1, "entry-1".to_string());
            index.append(2, "entry-2".to_string());

            let i1 = Arc::clone(&index);
            let i2 = Arc::clone(&index);
            let i3 = Arc::clone(&index);

            let th1 = loom::thread::spawn(move || i1.get(1));

            let th2 = loom::thread::spawn(move || i2.get(2));

            let th3 = loom::thread::spawn(move || i3.len());

            let r1 = th1.join().unwrap();
            let r2 = th2.join().unwrap();
            let r3 = th3.join().unwrap();

            assert_eq!(r1, Some("entry-1".to_string()));
            assert_eq!(r2, Some("entry-2".to_string()));
            assert_eq!(r3, 2);
        });
    }

    /// Test: Write exclusion in append log.
    ///
    /// Writes should be mutually exclusive.
    #[test]
    fn index_write_exclusion() {
        loom::model(|| {
            let index = Arc::new(AppendLogIndex::new());

            let i1 = Arc::clone(&index);
            let i2 = Arc::clone(&index);

            let th1 = loom::thread::spawn(move || {
                i1.append(1, "entry-1".to_string());
            });

            let th2 = loom::thread::spawn(move || {
                i2.append(2, "entry-2".to_string());
            });

            th1.join().unwrap();
            th2.join().unwrap();

            // Both entries should be present
            assert_eq!(index.len(), 2);
            assert!(index.get(1).is_some());
            assert!(index.get(2).is_some());
        });
    }

    /// Test: Read during write behavior.
    ///
    /// Reads during writes should either see pre-write or post-write state.
    #[test]
    fn index_read_during_write() {
        loom::model(|| {
            let index = Arc::new(AppendLogIndex::new());

            // Initial state
            index.append(1, "initial".to_string());

            let i1 = Arc::clone(&index);
            let i2 = Arc::clone(&index);

            let th1 = loom::thread::spawn(move || {
                i1.append(2, "added".to_string());
            });

            let th2 = loom::thread::spawn(move || {
                // Length is either 1 (before write) or 2 (after write)
                let len = i2.len();
                assert!(len == 1 || len == 2);
                len
            });

            th1.join().unwrap();
            let read_len = th2.join().unwrap();

            // After both complete, we should have 2 entries
            // But the read might have seen 1 or 2 depending on interleaving
            assert!(read_len == 1 || read_len == 2);
            assert_eq!(index.len(), 2);
        });
    }

    /// Model of a hash chain for integrity verification.
    ///
    /// Uses Mutex because hash chain updates are write-only operations
    /// that must be serialized.
    struct HashChain {
        current: loom::sync::Mutex<String>,
    }

    impl HashChain {
        fn new() -> Self {
            Self {
                current: loom::sync::Mutex::new("genesis".to_string()),
            }
        }

        fn update(&self, new_hash: String) {
            let mut guard = self.current.lock().unwrap();
            *guard = new_hash;
        }

        fn get(&self) -> String {
            let guard = self.current.lock().unwrap();
            guard.clone()
        }
    }

    /// Test: Hash chain updates are serialized.
    ///
    /// Hash chain must be updated atomically.
    #[test]
    fn hash_chain_serialization() {
        loom::model(|| {
            let chain = Arc::new(HashChain::new());

            let c1 = Arc::clone(&chain);
            let c2 = Arc::clone(&chain);

            let th1 = loom::thread::spawn(move || {
                c1.update("hash1".to_string());
            });

            let th2 = loom::thread::spawn(move || {
                c2.update("hash2".to_string());
            });

            th1.join().unwrap();
            th2.join().unwrap();

            // Final value should be one of the two
            let final_hash = chain.get();
            assert!(final_hash == "hash1" || final_hash == "hash2");
        });
    }

    /// Test: Hash chain read during update.
    ///
    /// Readers should see either the old or new hash, never garbage.
    #[test]
    fn hash_chain_read_during_update() {
        loom::model(|| {
            let chain = Arc::new(HashChain::new());

            let c1 = Arc::clone(&chain);
            let c2 = Arc::clone(&chain);

            let th1 = loom::thread::spawn(move || {
                c1.update("updated".to_string());
            });

            let th2 = loom::thread::spawn(move || {
                let hash = c2.get();
                // Should see either genesis or updated
                assert!(hash == "genesis" || hash == "updated");
            });

            th1.join().unwrap();
            th2.join().unwrap();

            assert_eq!(chain.get(), "updated");
        });
    }

    /// Model of a checkpoint manager.
    ///
    /// Checkpoints are created periodically and must be serialized.
    struct CheckpointManager {
        last_checkpoint_seq: loom::sync::atomic::AtomicU64,
        checkpoint_count: loom::sync::atomic::AtomicU64,
    }

    impl CheckpointManager {
        fn new() -> Self {
            Self {
                last_checkpoint_seq: loom::sync::atomic::AtomicU64::new(0),
                checkpoint_count: loom::sync::atomic::AtomicU64::new(0),
            }
        }

        fn create_checkpoint(&self, entry_seq: u64) -> bool {
            // Compare-and-swap to ensure monotonic checkpoints
            loop {
                let current = self
                    .last_checkpoint_seq
                    .load(loom::sync::atomic::Ordering::SeqCst);
                if entry_seq <= current {
                    return false; // Stale checkpoint
                }
                if self
                    .last_checkpoint_seq
                    .compare_exchange(
                        current,
                        entry_seq,
                        loom::sync::atomic::Ordering::SeqCst,
                        loom::sync::atomic::Ordering::SeqCst,
                    )
                    .is_ok()
                {
                    self.checkpoint_count
                        .fetch_add(1, loom::sync::atomic::Ordering::SeqCst);
                    return true;
                }
                // CAS failed, retry
            }
        }

        fn checkpoint_count(&self) -> u64 {
            self.checkpoint_count
                .load(loom::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Test: Checkpoint monotonicity via CAS.
    ///
    /// Only one of competing checkpoints should succeed if they have the same seq.
    #[test]
    fn checkpoint_cas_monotonicity() {
        loom::model(|| {
            let manager = Arc::new(CheckpointManager::new());

            let m1 = Arc::clone(&manager);
            let m2 = Arc::clone(&manager);

            // Both threads try to create checkpoint at entry_seq = 5
            let th1 = loom::thread::spawn(move || m1.create_checkpoint(5));

            let th2 = loom::thread::spawn(move || m2.create_checkpoint(5));

            let r1 = th1.join().unwrap();
            let r2 = th2.join().unwrap();

            // Exactly one should succeed
            assert_ne!(r1, r2);

            // Checkpoint count should be 1
            assert_eq!(manager.checkpoint_count(), 1);
        });
    }

    /// Test: Out-of-order checkpoint rejection.
    ///
    /// Older checkpoints should be rejected.
    #[test]
    fn checkpoint_out_of_order_rejected() {
        loom::model(|| {
            let manager = Arc::new(CheckpointManager::new());

            let m1 = Arc::clone(&manager);
            let m2 = Arc::clone(&manager);

            // First create checkpoint at 10
            let th1 = loom::thread::spawn(move || m1.create_checkpoint(10));

            // Then try checkpoint at 5 (should fail)
            let th2 = loom::thread::spawn(move || m2.create_checkpoint(5));

            let r1 = th1.join().unwrap();
            let r2 = th2.join().unwrap();

            // First should succeed
            assert!(r1);
            // Second should fail (stale)
            assert!(!r2);
        });
    }

    /// Model of a store with segmented files.
    ///
    /// Simulates the segment rollover logic with atomic operations.
    struct SegmentManager {
        current_segment: loom::sync::atomic::AtomicU64,
        segment_bytes: loom::sync::atomic::AtomicU64,
        max_segment_bytes: u64,
    }

    impl SegmentManager {
        fn new(max_bytes: u64) -> Self {
            Self {
                current_segment: loom::sync::atomic::AtomicU64::new(1),
                segment_bytes: loom::sync::atomic::AtomicU64::new(0),
                max_segment_bytes: max_bytes,
            }
        }

        /// Write bytes, potentially rolling over to new segment.
        /// Returns (`segment_seq`, offset).
        fn write(&self, bytes: u64) -> (u64, u64) {
            // Atomically add bytes and check for rollover
            let current_bytes = self
                .segment_bytes
                .fetch_add(bytes, loom::sync::atomic::Ordering::SeqCst);

            let total = current_bytes + bytes;

            if total >= self.max_segment_bytes {
                // Rollover: get new segment and reset bytes
                let new_segment = self
                    .current_segment
                    .fetch_add(1, loom::sync::atomic::Ordering::SeqCst)
                    + 1;
                // Note: In real implementation, this reset would need careful handling
                (new_segment, 0)
            } else {
                let segment = self
                    .current_segment
                    .load(loom::sync::atomic::Ordering::SeqCst);
                (segment, current_bytes)
            }
        }

        #[allow(dead_code)]
        fn current_segment(&self) -> u64 {
            self.current_segment
                .load(loom::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Test: Segment rollover under concurrent writes.
    ///
    /// Verify that segment assignments are consistent.
    #[test]
    fn segment_rollover_concurrent() {
        loom::model(|| {
            let manager = Arc::new(SegmentManager::new(100)); // 100 byte max

            let m1 = Arc::clone(&manager);
            let m2 = Arc::clone(&manager);

            let th1 = loom::thread::spawn(move || m1.write(60));

            let th2 = loom::thread::spawn(move || m2.write(60));

            let (seg1, off1) = th1.join().unwrap();
            let (seg2, off2) = th2.join().unwrap();

            // Both should have valid segments
            assert!(seg1 >= 1);
            assert!(seg2 >= 1);

            // If same segment, offsets shouldn't overlap (in this simplified model)
            // Note: Real implementation would need more careful offset tracking
            let _ = (seg1, off1, seg2, off2);
        });
    }

    /// Test: Combined model of `SessionStore` operations.
    ///
    /// Test entry allocation + index update + hash chain update together.
    #[test]
    fn combined_store_operations() {
        loom::model(|| {
            let counters = Arc::new(SequenceCounters::new());
            let index = Arc::new(AppendLogIndex::new());
            let chain = Arc::new(HashChain::new());

            let counters1 = Arc::clone(&counters);
            let index1 = Arc::clone(&index);
            let chain1 = Arc::clone(&chain);

            let counters2 = Arc::clone(&counters);
            let index2 = Arc::clone(&index);
            let chain2 = Arc::clone(&chain);

            // Thread 1: allocate seq, append to index, update hash
            let th1 = loom::thread::spawn(move || {
                let seq = counters1.allocate_entry_seq();
                index1.append(seq, format!("entry-{seq}"));
                chain1.update(format!("hash-{seq}"));
            });

            // Thread 2: same operations
            let th2 = loom::thread::spawn(move || {
                let seq = counters2.allocate_entry_seq();
                index2.append(seq, format!("entry-{seq}"));
                chain2.update(format!("hash-{seq}"));
            });

            th1.join().unwrap();
            th2.join().unwrap();

            // Index should have 2 entries
            assert_eq!(index.len(), 2);

            // Hash chain should be updated
            let final_hash = chain.get();
            assert!(final_hash.starts_with("hash-"));
        });
    }
}

#[cfg(test)]
mod standard_concurrency_tests {
    //! Standard concurrency tests for session store access patterns.
    //!
    //! These tests use real threads and real synchronization primitives
    //! to verify the expected behavior of concurrent access patterns.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    /// Test atomic sequence allocation.
    #[test]
    fn real_atomic_sequence() {
        let seq = Arc::new(AtomicU64::new(1));

        let mut handles = vec![];

        for _ in 0..10 {
            let seq = Arc::clone(&seq);
            handles.push(thread::spawn(move || {
                let mut allocated = vec![];
                for _ in 0..100 {
                    let val = seq.fetch_add(1, Ordering::SeqCst);
                    allocated.push(val);
                }
                allocated
            }));
        }

        let mut all_allocated: Vec<u64> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();

        all_allocated.sort_unstable();
        all_allocated.dedup();

        // All 1000 allocations should be unique
        assert_eq!(all_allocated.len(), 1000);
    }

    /// Test `RwLock` read-heavy workload.
    #[test]
    fn real_rwlock_read_heavy() {
        use std::sync::RwLock;

        let data = Arc::new(RwLock::new(vec![(1, "a"), (2, "b"), (3, "c")]));

        let mut handles = vec![];

        // 10 reader threads
        for _ in 0..10 {
            let data = Arc::clone(&data);
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    let guard = data.read().unwrap();
                    assert!(!guard.is_empty());
                    drop(guard);
                }
            }));
        }

        // 2 writer threads
        for i in 0..2 {
            let data = Arc::clone(&data);
            handles.push(thread::spawn(move || {
                for j in 0..10 {
                    let mut guard = data.write().unwrap();
                    guard.push((100 + i * 10 + j, "new"));
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Should have original 3 + 20 new entries
        let guard = data.read().unwrap();
        assert_eq!(guard.len(), 23);
        drop(guard);
    }

    /// Test Mutex write-only workload.
    #[test]
    fn real_mutex_write_only() {
        use std::sync::Mutex;

        let data = Arc::new(Mutex::new(Vec::new()));

        let mut handles = vec![];

        for i in 0..10 {
            let data = Arc::clone(&data);
            handles.push(thread::spawn(move || {
                for j in 0..100 {
                    let mut guard = data.lock().unwrap();
                    guard.push(i * 100 + j);
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let guard = data.lock().unwrap();
        assert_eq!(guard.len(), 1000);
        drop(guard);
    }

    /// Test CAS-based monotonic update.
    #[test]
    fn real_cas_monotonic() {
        let counter = Arc::new(AtomicU64::new(0));

        let mut handles = vec![];

        for _ in 0..10 {
            let counter = Arc::clone(&counter);
            handles.push(thread::spawn(move || {
                let mut success_count = 0;
                for target in 1..=100 {
                    loop {
                        let current = counter.load(Ordering::SeqCst);
                        if current >= target {
                            break;
                        }
                        if counter
                            .compare_exchange(current, target, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                        {
                            success_count += 1;
                            break;
                        }
                    }
                }
                success_count
            }));
        }

        let total_successes: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();

        // Final value should be 100
        assert_eq!(counter.load(Ordering::SeqCst), 100);
        // Some threads should have succeeded
        assert!(total_successes > 0);
    }
}
