//! Simple in-memory cache for web content with TTL.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// A cached web response.
#[derive(Debug, Clone)]
pub struct CachedEntry {
    pub content: String,
    pub fetched_at: Instant,
    pub ttl_secs: u64,
    pub final_url: String,
    pub status: u16,
}

impl CachedEntry {
    /// Check if this entry has expired.
    pub fn is_expired(&self) -> bool {
        self.fetched_at.elapsed().as_secs() > self.ttl_secs
    }
}

/// Simple web content cache.
#[derive(Debug, Clone)]
pub struct WebCache {
    entries: Arc<Mutex<HashMap<String, CachedEntry>>>,
    default_ttl_secs: u64,
    max_entries: usize,
}

impl Default for WebCache {
    fn default() -> Self {
        Self::new(3600, 100) // 1 hour TTL, 100 entries max
    }
}

impl WebCache {
    pub fn new(default_ttl_secs: u64, max_entries: usize) -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::new())),
            default_ttl_secs,
            max_entries,
        }
    }

    /// Get cached content for a URL.
    pub fn get(&self, url: &str) -> Option<CachedEntry> {
        let entries = self.entries.lock().ok()?;
        let entry = entries.get(url)?;
        if entry.is_expired() {
            return None;
        }
        Some(entry.clone())
    }

    /// Store content in cache.
    pub fn set(&self, url: &str, content: String, final_url: String, status: u16) {
        if let Ok(mut entries) = self.entries.lock() {
            // Evict expired entries if at capacity
            if entries.len() >= self.max_entries {
                entries.retain(|_, e| !e.is_expired());
            }
            // If still at capacity, remove oldest
            if entries.len() >= self.max_entries {
                if let Some((oldest_url, _)) = entries.iter().min_by_key(|(_, e)| e.fetched_at) {
                    let oldest_url = oldest_url.clone();
                    entries.remove(&oldest_url);
                }
            }

            entries.insert(
                url.to_string(),
                CachedEntry {
                    content,
                    fetched_at: Instant::now(),
                    ttl_secs: self.default_ttl_secs,
                    final_url,
                    status,
                },
            );
        }
    }

    /// Clear all cached entries.
    pub fn clear(&self) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.clear();
        }
    }

    /// Get cache statistics.
    pub fn stats(&self) -> CacheStats {
        if let Ok(entries) = self.entries.lock() {
            let total = entries.len();
            let expired = entries.values().filter(|e| e.is_expired()).count();
            CacheStats {
                total,
                active: total - expired,
                expired,
            }
        } else {
            CacheStats {
                total: 0,
                active: 0,
                expired: 0,
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct CacheStats {
    pub total: usize,
    pub active: usize,
    pub expired: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_cache_default() {
        let cache = WebCache::default();
        let stats = cache.stats();
        assert_eq!(stats.total, 0);
    }

    #[test]
    fn web_cache_set_and_get() {
        let cache = WebCache::new(3600, 100);
        cache.set(
            "https://example.com",
            "content".to_string(),
            "https://example.com".to_string(),
            200,
        );

        let entry = cache.get("https://example.com").unwrap();
        assert_eq!(entry.content, "content");
        assert_eq!(entry.status, 200);
    }

    #[test]
    fn web_cache_miss() {
        let cache = WebCache::default();
        assert!(cache.get("https://missing.com").is_none());
    }

    #[test]
    fn web_cache_clear() {
        let cache = WebCache::default();
        cache.set(
            "https://example.com",
            "content".to_string(),
            "https://example.com".to_string(),
            200,
        );
        assert!(cache.get("https://example.com").is_some());
        cache.clear();
        assert!(cache.get("https://example.com").is_none());
    }

    #[test]
    fn web_cache_stats() {
        let cache = WebCache::new(3600, 100);
        cache.set(
            "https://a.com",
            "a".to_string(),
            "https://a.com".to_string(),
            200,
        );
        cache.set(
            "https://b.com",
            "b".to_string(),
            "https://b.com".to_string(),
            200,
        );

        let stats = cache.stats();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.active, 2);
        assert_eq!(stats.expired, 0);
    }

    #[test]
    fn cached_entry_not_expired() {
        let entry = CachedEntry {
            content: "test".to_string(),
            fetched_at: Instant::now(),
            ttl_secs: 3600,
            final_url: "https://example.com".to_string(),
            status: 200,
        };
        assert!(!entry.is_expired());
    }
}
