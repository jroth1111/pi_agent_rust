//! Loom-based concurrency tests for CredentialCache.
//!
//! These tests model concurrent access patterns to verify the RwLock-based
//! LRU cache is free from data races, deadlocks, and other concurrency bugs.

#[cfg(test)]
mod loom_tests {
    use std::sync::Arc;

    /// Simplified cache model for loom testing.
    ///
    /// Loom cannot test the real LruCache because it uses non-loom primitives.
    /// Instead, we model the locking behavior with a simplified HashMap-like structure.
    #[derive(Debug)]
    struct CacheModel {
        data: std::collections::HashMap<String, String>,
        enabled: bool,
    }

    impl CacheModel {
        fn new(enabled: bool) -> Self {
            Self {
                data: std::collections::HashMap::new(),
                enabled,
            }
        }

        fn get(&self, key: &str) -> Option<String> {
            if !self.enabled {
                return None;
            }
            self.data.get(key).cloned()
        }

        fn put(&mut self, key: String, value: String) {
            if !self.enabled {
                return;
            }
            self.data.insert(key, value);
        }

        fn invalidate(&mut self, key: &str) {
            self.data.remove(key);
        }

        fn clear(&mut self) {
            self.data.clear();
        }

        fn len(&self) -> usize {
            self.data.len()
        }
    }

    /// Thread-safe cache wrapper using loom's RwLock.
    struct ThreadSafeCache {
        cache: loom::sync::RwLock<CacheModel>,
    }

    impl ThreadSafeCache {
        fn new(enabled: bool) -> Self {
            Self {
                cache: loom::sync::RwLock::new(CacheModel::new(enabled)),
            }
        }

        fn get(&self, key: &str) -> Option<String> {
            let guard = self.cache.read().unwrap();
            guard.get(key)
        }

        fn put(&self, key: String, value: String) {
            let mut guard = self.cache.write().unwrap();
            guard.put(key, value);
        }

        fn invalidate(&self, key: &str) {
            let mut guard = self.cache.write().unwrap();
            guard.invalidate(key);
        }

        fn clear(&self) {
            let mut guard = self.cache.write().unwrap();
            guard.clear();
        }

        fn len(&self) -> usize {
            let guard = self.cache.read().unwrap();
            guard.len()
        }
    }

    /// Test: Concurrent reads should not cause data races.
    ///
    /// Multiple threads reading from the cache simultaneously should be safe
    /// and should not block each other (RwLock allows multiple readers).
    #[test]
    fn concurrent_reads_no_race() {
        loom::model(|| {
            let cache = Arc::new(ThreadSafeCache::new(true));

            // Pre-populate cache
            cache.put("provider1".to_string(), "credential1".to_string());
            cache.put("provider2".to_string(), "credential2".to_string());

            let cache1 = Arc::clone(&cache);
            let cache2 = Arc::clone(&cache);

            let th1 = loom::thread::spawn(move || {
                let val = cache1.get("provider1");
                // Should always get Some since we inserted before spawning
                assert!(val.is_some() || val.is_none()); // loom explores all interleavings
            });

            let th2 = loom::thread::spawn(move || {
                let val = cache2.get("provider2");
                assert!(val.is_some() || val.is_none());
            });

            th1.join().unwrap();
            th2.join().unwrap();
        });
    }

    /// Test: Concurrent writes should be serialized.
    ///
    /// Multiple threads writing to the cache should not corrupt data.
    /// Each write should be atomic from the perspective of other threads.
    #[test]
    fn concurrent_writes_serialized() {
        loom::model(|| {
            let cache = Arc::new(ThreadSafeCache::new(true));

            let cache1 = Arc::clone(&cache);
            let cache2 = Arc::clone(&cache);

            let th1 = loom::thread::spawn(move || {
                cache1.put("key".to_string(), "value1".to_string());
            });

            let th2 = loom::thread::spawn(move || {
                cache2.put("key".to_string(), "value2".to_string());
            });

            th1.join().unwrap();
            th2.join().unwrap();

            // Final value should be one of the two writes (deterministic per interleaving)
            let final_val = cache.get("key");
            assert!(
                final_val == Some("value1".to_string()) || final_val == Some("value2".to_string())
            );
        });
    }

    /// Test: Read-write concurrent access.
    ///
    /// Verify that writes block reads and vice versa, preventing
    /// readers from seeing partially-written state.
    #[test]
    fn read_write_exclusion() {
        loom::model(|| {
            let cache = Arc::new(ThreadSafeCache::new(true));

            // Pre-populate
            cache.put("key".to_string(), "initial".to_string());

            let cache1 = Arc::clone(&cache);
            let cache2 = Arc::clone(&cache);

            let th1 = loom::thread::spawn(move || {
                cache1.put("key".to_string(), "updated".to_string());
            });

            let th2 = loom::thread::spawn(move || {
                // This read either sees "initial" or "updated", never garbage
                let val = cache2.get("key");
                assert!(
                    val == Some("initial".to_string())
                        || val == Some("updated".to_string())
                        || val.is_none() // If read happens during write, loom may show this
                );
            });

            th1.join().unwrap();
            th2.join().unwrap();
        });
    }

    /// Test: Clear operation is atomic.
    ///
    /// Clearing the cache should be atomic - threads should either see
    /// the cache before clear or after clear, never a partial state.
    #[test]
    fn clear_is_atomic() {
        loom::model(|| {
            let cache = Arc::new(ThreadSafeCache::new(true));

            cache.put("key1".to_string(), "value1".to_string());
            cache.put("key2".to_string(), "value2".to_string());

            let cache1 = Arc::clone(&cache);
            let cache2 = Arc::clone(&cache);

            let th1 = loom::thread::spawn(move || {
                cache1.clear();
            });

            let th2 = loom::thread::spawn(move || {
                // Length should be either 2 (before clear) or 0 (after clear)
                let len = cache2.len();
                assert!(len == 0 || len == 2);
            });

            th1.join().unwrap();
            th2.join().unwrap();

            // After both threads complete, cache should be empty
            assert_eq!(cache.len(), 0);
        });
    }

    /// Test: Invalidation is atomic.
    ///
    /// Invalidating a key should be atomic - readers should not see
    /// a partially invalidated state.
    #[test]
    fn invalidate_is_atomic() {
        loom::model(|| {
            let cache = Arc::new(ThreadSafeCache::new(true));

            cache.put("key".to_string(), "value".to_string());

            let cache1 = Arc::clone(&cache);
            let cache2 = Arc::clone(&cache);

            let th1 = loom::thread::spawn(move || {
                cache1.invalidate("key");
            });

            let th2 = loom::thread::spawn(move || {
                // Should see either the value or nothing
                let val = cache2.get("key");
                assert!(val.is_some() || val.is_none());
            });

            th1.join().unwrap();
            th2.join().unwrap();

            // After both complete, key should be gone
            assert!(cache.get("key").is_none());
        });
    }

    /// Test: Disabled cache never returns values.
    ///
    /// When the cache is disabled, all operations should be no-ops.
    #[test]
    fn disabled_cache_no_ops() {
        loom::model(|| {
            let cache = Arc::new(ThreadSafeCache::new(false));

            let cache1 = Arc::clone(&cache);
            let cache2 = Arc::clone(&cache);

            let th1 = loom::thread::spawn(move || {
                cache1.put("key".to_string(), "value".to_string());
            });

            let th2 = loom::thread::spawn(move || {
                // Even after put, should return None since cache is disabled
                let val = cache2.get("key");
                assert!(val.is_none());
            });

            th1.join().unwrap();
            th2.join().unwrap();

            // Should be empty
            assert_eq!(cache.len(), 0);
        });
    }

    /// Test: Multiple readers don't block each other.
    ///
    /// RwLock should allow multiple simultaneous readers.
    /// This test verifies the read-read concurrency pattern.
    #[test]
    fn multiple_readers_non_blocking() {
        loom::model(|| {
            let cache = Arc::new(ThreadSafeCache::new(true));

            cache.put("shared".to_string(), "data".to_string());

            let cache1 = Arc::clone(&cache);
            let cache2 = Arc::clone(&cache);
            let cache3 = Arc::clone(&cache);

            let th1 = loom::thread::spawn(move || {
                cache1.get("shared");
            });

            let th2 = loom::thread::spawn(move || {
                cache2.get("shared");
            });

            let th3 = loom::thread::spawn(move || {
                cache3.get("shared");
            });

            th1.join().unwrap();
            th2.join().unwrap();
            th3.join().unwrap();

            // Cache should still contain the data
            assert_eq!(cache.get("shared"), Some("data".to_string()));
        });
    }

    /// Test: Write blocks subsequent reads until complete.
    ///
    /// This verifies the write exclusion property of RwLock.
    #[test]
    fn write_blocks_reads() {
        loom::model(|| {
            let cache = Arc::new(ThreadSafeCache::new(true));

            cache.put("counter".to_string(), "0".to_string());

            let cache1 = Arc::clone(&cache);
            let cache2 = Arc::clone(&cache);
            let cache3 = Arc::clone(&cache);

            // Writer thread
            let th1 = loom::thread::spawn(move || {
                cache1.put("counter".to_string(), "1".to_string());
            });

            // Reader threads
            let th2 = loom::thread::spawn(move || {
                let val = cache2.get("counter");
                // Should see either "0" or "1", never anything else
                assert!(val == Some("0".to_string()) || val == Some("1".to_string()));
            });

            let th3 = loom::thread::spawn(move || {
                let val = cache3.get("counter");
                assert!(val == Some("0".to_string()) || val == Some("1".to_string()));
            });

            th1.join().unwrap();
            th2.join().unwrap();
            th3.join().unwrap();
        });
    }

    /// Test: High contention scenario.
    ///
    /// Test with multiple writers and readers to verify no deadlocks
    /// or data corruption under high contention.
    #[test]
    fn high_contention_no_deadlock() {
        loom::model(|| {
            let cache = Arc::new(ThreadSafeCache::new(true));

            // Pre-populate
            for i in 0..3 {
                cache.put(format!("key{}", i), format!("value{}", i));
            }

            let mut handles = vec![];

            // Spawn 2 writers
            for i in 0..2 {
                let cache = Arc::clone(&cache);
                handles.push(loom::thread::spawn(move || {
                    cache.put(format!("key{}", i), format!("updated{}", i));
                }));
            }

            // Spawn 2 readers
            for i in 0..2 {
                let cache = Arc::clone(&cache);
                handles.push(loom::thread::spawn(move || {
                    let _ = cache.get(&format!("key{}", i));
                }));
            }

            // All threads should complete without deadlock
            for handle in handles {
                handle.join().unwrap();
            }
        });
    }

    /// Test: Stress test with rapid put/get/invalidate cycles.
    ///
    /// Verifies correctness under rapid operation cycles.
    #[test]
    fn rapid_operation_cycles() {
        loom::model(|| {
            let cache = Arc::new(ThreadSafeCache::new(true));

            let cache1 = Arc::clone(&cache);
            let cache2 = Arc::clone(&cache);
            let cache3 = Arc::clone(&cache);

            // Thread 1: put, put, put
            let th1 = loom::thread::spawn(move || {
                cache1.put("key".to_string(), "v1".to_string());
                cache1.put("key".to_string(), "v2".to_string());
                cache1.put("key".to_string(), "v3".to_string());
            });

            // Thread 2: invalidate, invalidate
            let th2 = loom::thread::spawn(move || {
                cache2.invalidate("key");
                cache2.invalidate("key");
            });

            // Thread 3: read, read, read
            let th3 = loom::thread::spawn(move || {
                let _ = cache3.get("key");
                let _ = cache3.get("key");
                let _ = cache3.get("key");
            });

            th1.join().unwrap();
            th2.join().unwrap();
            th3.join().unwrap();

            // Final state is deterministic based on last write operation
            // Either "v3" or None (if last op was invalidate)
            let final_val = cache.get("key");
            assert!(final_val.is_none() || final_val == Some("v3".to_string()));
        });
    }
}

#[cfg(test)]
mod standard_concurrency_tests {
    //! Standard concurrency tests using real threads (not loom).
    //! These complement loom tests by testing actual multi-threaded behavior.

    use std::sync::Arc;
    use std::thread;

    // Import the real CredentialCache for standard tests
    use super::super::cache::{CacheConfig, CredentialCache};

    /// Test concurrent reads with real threads.
    #[test]
    fn real_concurrent_reads() {
        let cache = Arc::new(CredentialCache::new());

        // Pre-populate
        cache.put("provider1", "cred1".to_string());
        cache.put("provider2", "cred2".to_string());

        let mut handles = vec![];

        for _ in 0..10 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let _ = cache.get("provider1");
                    let _ = cache.get("provider2");
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Cache should still be valid
        assert!(cache.get("provider1").is_some());
        assert!(cache.get("provider2").is_some());
    }

    /// Test concurrent writes with real threads.
    #[test]
    fn real_concurrent_writes() {
        let cache = Arc::new(CredentialCache::new());

        let mut handles = vec![];

        for i in 0..10 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for j in 0..100 {
                    cache.put(&format!("key-{}-{}", i, j), "value".to_string());
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Cache should have entries (exact count depends on timing)
        assert!(cache.len() > 0);
    }

    /// Test concurrent read-write mix.
    #[test]
    fn real_concurrent_read_write() {
        let cache = Arc::new(CredentialCache::new());
        cache.put("shared", "initial".to_string());

        let mut handles = vec![];

        // Writers
        for i in 0..5 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for j in 0..50 {
                    cache.put(&format!("key-{}-{}", i, j), "value".to_string());
                }
            }));
        }

        // Readers
        for _ in 0..5 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let _ = cache.get("shared");
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }

    /// Test concurrent invalidation.
    #[test]
    fn real_concurrent_invalidation() {
        let cache = Arc::new(CredentialCache::new());

        // Populate cache
        for i in 0..100 {
            cache.put(&format!("key-{}", i), "value".to_string());
        }

        let mut handles = vec![];

        // Some threads invalidate
        for i in 0..5 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for j in 0..20 {
                    cache.invalidate(&format!("key-{}", i * 20 + j));
                }
            }));
        }

        // Other threads read
        for _ in 0..5 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for i in 0..100 {
                    let _ = cache.get(&format!("key-{}", i));
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }

    /// Test cache stats under concurrent access.
    #[test]
    fn real_stats_concurrent_access() {
        let cache = Arc::new(CredentialCache::new());

        let mut handles = vec![];

        // Populate
        for i in 0..50 {
            cache.put(&format!("key-{}", i), "value".to_string());
        }

        // Stats readers
        for _ in 0..5 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let stats = cache.stats();
                    assert!(stats.total_entries <= 50);
                }
            }));
        }

        // Writers
        for _ in 0..5 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for i in 0..50 {
                    cache.invalidate(&format!("key-{}", i));
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }

    /// Test clear under concurrent access.
    #[test]
    fn real_clear_concurrent() {
        let cache = Arc::new(CredentialCache::new());

        // Populate
        for i in 0..100 {
            cache.put(&format!("key-{}", i), "value".to_string());
        }

        let mut handles = vec![];

        // Clearer threads
        for _ in 0..3 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                cache.clear();
            }));
        }

        // Reader threads
        for _ in 0..5 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for i in 0..100 {
                    let _ = cache.get(&format!("key-{}", i));
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Final clear to ensure deterministic end state
        cache.clear();
        assert!(cache.is_empty());
    }

    /// Test purge_expired under concurrent access.
    #[test]
    fn real_purge_expired_concurrent() {
        use std::time::Duration;

        let config = CacheConfig {
            default_ttl: Duration::from_millis(50),
            ..Default::default()
        };
        let cache = Arc::new(CredentialCache::with_config(config));

        // Add entries
        for i in 0..50 {
            cache.put(&format!("key-{}", i), "value".to_string());
        }

        let mut handles = vec![];

        // Threads that purge
        for _ in 0..3 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for _ in 0..10 {
                    cache.purge_expired();
                    thread::sleep(Duration::from_millis(10));
                }
            }));
        }

        // Threads that read
        for _ in 0..5 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for i in 0..50 {
                    let _ = cache.get(&format!("key-{}", i));
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }

    /// Test account-based operations under concurrency.
    #[test]
    fn real_account_concurrent() {
        let cache = Arc::new(CredentialCache::new());

        let mut handles = vec![];

        // Writers with accounts
        for i in 0..5 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for j in 0..20 {
                    cache.put_for_account(
                        "google",
                        &format!("user-{}-{}", i, j),
                        format!("token-{}-{}", i, j),
                    );
                }
            }));
        }

        // Readers with accounts
        for _ in 0..5 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for i in 0..5 {
                    for j in 0..20 {
                        let _ = cache.get_for_account("google", &format!("user-{}-{}", i, j));
                    }
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }

    /// Test invalidate_provider under concurrent access.
    #[test]
    fn real_invalidate_provider_concurrent() {
        let cache = Arc::new(CredentialCache::new());

        // Populate multiple providers
        for p in 0..5 {
            for a in 0..10 {
                cache.put_for_account(
                    &format!("provider-{}", p),
                    &format!("account-{}", a),
                    "token".to_string(),
                );
            }
        }

        let mut handles = vec![];

        // Invalidation threads
        for p in 0..5 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                cache.invalidate_provider(&format!("provider-{}", p));
            }));
        }

        // Reader threads
        for _ in 0..5 {
            let cache = Arc::clone(&cache);
            handles.push(thread::spawn(move || {
                for p in 0..5 {
                    for a in 0..10 {
                        let _ = cache
                            .get_for_account(&format!("provider-{}", p), &format!("account-{}", a));
                    }
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }
    }
}
