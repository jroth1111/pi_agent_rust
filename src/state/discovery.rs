//! Discovery tracking for scope management.
//!
//! This module provides tracking for work discovered during task execution.
//! It addresses scope creep by enforcing budgets on discovered work items
//! and categorizing them by priority. This supports the Robustness Layer's
//! Phase 2 scope management requirements.

use serde::{Deserialize, Serialize};

/// Priority of a discovered item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum DiscoveryPriority {
    /// Must be addressed - blocks current task.
    MustAddress,
    /// Should address - related but not blocking.
    #[default]
    ShouldAddress,
    /// Nice to have - improvement opportunity.
    NiceToHave,
}

impl DiscoveryPriority {
    /// Returns true if this priority blocks task completion.
    pub const fn is_blocking(&self) -> bool {
        matches!(self, Self::MustAddress)
    }

    /// Returns the numeric weight for sorting (higher = more important).
    pub const fn weight(&self) -> u8 {
        match self {
            Self::MustAddress => 3,
            Self::ShouldAddress => 2,
            Self::NiceToHave => 1,
        }
    }
}

/// A discovered work item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Discovery {
    /// Unique identifier.
    pub id: String,
    /// Description of the discovery.
    pub description: String,
    /// Task that led to this discovery.
    pub source_task: String,
    /// Priority level.
    pub priority: DiscoveryPriority,
    /// Whether this has been addressed.
    pub addressed: bool,
    /// Timestamp when discovered.
    pub discovered_at: u64,
}

impl Discovery {
    /// Creates a new discovery with the given parameters.
    pub const fn new(
        id: String,
        description: String,
        source_task: String,
        priority: DiscoveryPriority,
        discovered_at: u64,
    ) -> Self {
        Self {
            id,
            description,
            source_task,
            priority,
            addressed: false,
            discovered_at,
        }
    }

    /// Returns true if this discovery is blocking.
    pub const fn is_blocking(&self) -> bool {
        self.priority.is_blocking() && !self.addressed
    }
}

/// Budget for scope expansion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeBudget {
    /// Maximum discoveries allowed.
    pub max_discoveries: usize,
    /// Maximum must-address items allowed.
    pub max_must_address: usize,
}

impl ScopeBudget {
    /// Creates a new scope budget with the given limits.
    pub const fn new(max_discoveries: usize, max_must_address: usize) -> Self {
        Self {
            max_discoveries,
            max_must_address,
        }
    }

    /// Creates a default budget (10 discoveries, 3 must-address).
    pub const fn default_limits() -> Self {
        Self {
            max_discoveries: 10,
            max_must_address: 3,
        }
    }

    /// Creates a restrictive budget for tight scope control.
    pub const fn restrictive() -> Self {
        Self {
            max_discoveries: 5,
            max_must_address: 1,
        }
    }

    /// Creates a permissive budget for exploratory work.
    pub const fn permissive() -> Self {
        Self {
            max_discoveries: 20,
            max_must_address: 5,
        }
    }
}

impl Default for ScopeBudget {
    fn default() -> Self {
        Self::default_limits()
    }
}

/// Summary statistics for discoveries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoverySummary {
    /// Total number of discoveries.
    pub total: usize,
    /// Number of unaddressed discoveries.
    pub unaddressed: usize,
    /// Number of must-address items (including addressed).
    pub must_address_total: usize,
    /// Number of unaddressed must-address items.
    pub must_address_unaddressed: usize,
    /// Number of should-address items (including addressed).
    pub should_address_total: usize,
    /// Number of unaddressed should-address items.
    pub should_address_unaddressed: usize,
    /// Number of nice-to-have items (including addressed).
    pub nice_to_have_total: usize,
    /// Number of unaddressed nice-to-have items.
    pub nice_to_have_unaddressed: usize,
    /// Whether over budget.
    pub over_budget: bool,
}

impl DiscoverySummary {
    /// Returns true if there are blocking (unaddressed must-address) items.
    pub const fn has_blocking(&self) -> bool {
        self.must_address_unaddressed > 0
    }

    /// Returns the total number of unaddressed items across all priorities.
    pub const fn total_unaddressed(&self) -> usize {
        self.must_address_unaddressed
            + self.should_address_unaddressed
            + self.nice_to_have_unaddressed
    }
}

/// Tracker for discovered work items.
///
/// Maintains a registry of work discovered during task execution,
/// enforces scope budgets, and provides summary statistics.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryTracker {
    discoveries: Vec<Discovery>,
    scope_budget: Option<ScopeBudget>,
    next_id: u64,
}

impl DiscoveryTracker {
    /// Creates a new empty discovery tracker.
    pub const fn new() -> Self {
        Self {
            discoveries: Vec::new(),
            scope_budget: None,
            next_id: 1,
        }
    }

    /// Creates a tracker with the given budget.
    pub const fn with_budget(budget: ScopeBudget) -> Self {
        Self {
            discoveries: Vec::new(),
            scope_budget: Some(budget),
            next_id: 1,
        }
    }

    /// Sets or updates the scope budget.
    pub const fn set_budget(&mut self, budget: ScopeBudget) {
        self.scope_budget = Some(budget);
    }

    /// Clears the scope budget (removes all limits).
    pub const fn clear_budget(&mut self) {
        self.scope_budget = None;
    }

    /// Generates a new unique discovery ID.
    fn generate_id(&mut self) -> String {
        let id = format!("disc-{}", self.next_id);
        self.next_id += 1;
        id
    }

    /// Adds a new discovery.
    ///
    /// Returns an error if the discovery would exceed the scope budget.
    pub fn discover(
        &mut self,
        description: &str,
        source_task: &str,
        priority: DiscoveryPriority,
    ) -> crate::error::Result<&Discovery> {
        // Check budget before adding
        if let Some(budget) = &self.scope_budget {
            let current_must_address = self.must_address_count();
            let is_must_address = matches!(priority, DiscoveryPriority::MustAddress);

            // Check must-address limit
            if is_must_address && current_must_address >= budget.max_must_address {
                return Err(crate::error::Error::Validation(format!(
                    "Cannot add must-address discovery: would exceed budget of {} (currently {})",
                    budget.max_must_address, current_must_address
                )));
            }

            // Check total discoveries limit
            if self.discoveries.len() >= budget.max_discoveries {
                return Err(crate::error::Error::Validation(format!(
                    "Cannot add discovery: would exceed budget of {} total discoveries",
                    budget.max_discoveries
                )));
            }
        }

        let discovery = Discovery::new(
            self.generate_id(),
            description.to_string(),
            source_task.to_string(),
            priority,
            current_timestamp_ms(),
        );

        self.discoveries.push(discovery);
        Ok(self.discoveries.last().expect("just pushed"))
    }

    /// Marks a discovery as addressed by ID.
    ///
    /// Returns true if the discovery was found and marked,
    /// false if the ID was not found.
    pub fn address(&mut self, id: &str) -> bool {
        if let Some(discovery) = self.discoveries.iter_mut().find(|d| d.id == id) {
            discovery.addressed = true;
            true
        } else {
            false
        }
    }

    /// Gets a discovery by ID.
    pub fn get(&self, id: &str) -> Option<&Discovery> {
        self.discoveries.iter().find(|d| d.id == id)
    }

    /// Returns all discoveries.
    pub fn all(&self) -> &[Discovery] {
        &self.discoveries
    }

    /// Returns all unaddressed discoveries, sorted by priority (descending).
    pub fn unaddressed(&self) -> Vec<&Discovery> {
        let mut unaddressed: Vec<_> = self.discoveries.iter().filter(|d| !d.addressed).collect();

        // Sort by priority weight (descending), then by discovery time (ascending)
        unaddressed.sort_by(|a, b| {
            b.priority
                .weight()
                .cmp(&a.priority.weight())
                .then_with(|| a.discovered_at.cmp(&b.discovered_at))
        });

        unaddressed
    }

    /// Returns discoveries filtered by priority.
    pub fn by_priority(&self, priority: DiscoveryPriority) -> Vec<&Discovery> {
        self.discoveries
            .iter()
            .filter(|d| d.priority == priority)
            .collect()
    }

    /// Returns the count of must-address discoveries (including addressed).
    pub fn must_address_count(&self) -> usize {
        self.discoveries
            .iter()
            .filter(|d| matches!(d.priority, DiscoveryPriority::MustAddress))
            .count()
    }

    /// Returns the count of unaddressed must-address discoveries.
    pub fn must_address_unaddressed_count(&self) -> usize {
        self.discoveries
            .iter()
            .filter(|d| d.priority == DiscoveryPriority::MustAddress && !d.addressed)
            .count()
    }

    /// Returns true if the current state exceeds the configured budget.
    pub fn is_over_budget(&self) -> bool {
        if let Some(budget) = &self.scope_budget {
            self.discoveries.len() > budget.max_discoveries
                || self.must_address_count() > budget.max_must_address
        } else {
            false
        }
    }

    /// Returns a summary of all discoveries.
    pub fn summary(&self) -> DiscoverySummary {
        let must_address: Vec<_> = self.by_priority(DiscoveryPriority::MustAddress);
        let should_address: Vec<_> = self.by_priority(DiscoveryPriority::ShouldAddress);
        let nice_to_have: Vec<_> = self.by_priority(DiscoveryPriority::NiceToHave);

        let must_address_unaddressed = must_address.iter().filter(|d| !d.addressed).count();
        let should_address_unaddressed = should_address.iter().filter(|d| !d.addressed).count();
        let nice_to_have_unaddressed = nice_to_have.iter().filter(|d| !d.addressed).count();

        DiscoverySummary {
            total: self.discoveries.len(),
            unaddressed: must_address_unaddressed
                + should_address_unaddressed
                + nice_to_have_unaddressed,
            must_address_total: must_address.len(),
            must_address_unaddressed,
            should_address_total: should_address.len(),
            should_address_unaddressed,
            nice_to_have_total: nice_to_have.len(),
            nice_to_have_unaddressed,
            over_budget: self.is_over_budget(),
        }
    }

    /// Clears all discoveries.
    pub fn clear(&mut self) {
        self.discoveries.clear();
        self.next_id = 1;
    }

    /// Removes addressed discoveries that are older than the given timestamp.
    ///
    /// Returns the number of discoveries removed.
    pub fn prune_addressed_before(&mut self, timestamp_ms: u64) -> usize {
        let original_len = self.discoveries.len();
        self.discoveries
            .retain(|d| !(d.addressed && d.discovered_at < timestamp_ms));
        original_len - self.discoveries.len()
    }
}

/// Helper to get current timestamp in milliseconds.
fn current_timestamp_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_priority_default() {
        let priority = DiscoveryPriority::default();
        assert_eq!(priority, DiscoveryPriority::ShouldAddress);
    }

    #[test]
    fn discovery_priority_blocking() {
        assert!(DiscoveryPriority::MustAddress.is_blocking());
        assert!(!DiscoveryPriority::ShouldAddress.is_blocking());
        assert!(!DiscoveryPriority::NiceToHave.is_blocking());
    }

    #[test]
    fn discovery_priority_weight() {
        assert_eq!(DiscoveryPriority::MustAddress.weight(), 3);
        assert_eq!(DiscoveryPriority::ShouldAddress.weight(), 2);
        assert_eq!(DiscoveryPriority::NiceToHave.weight(), 1);
    }

    #[test]
    fn discovery_tracker_new() {
        let tracker = DiscoveryTracker::new();
        assert!(tracker.all().is_empty());
        assert_eq!(tracker.must_address_count(), 0);
        assert!(!tracker.is_over_budget());
    }

    #[test]
    fn discovery_tracker_with_budget() {
        let budget = ScopeBudget::new(5, 2);
        let tracker = DiscoveryTracker::with_budget(budget);
        assert!(!tracker.is_over_budget());
    }

    #[test]
    fn discover_adds_item() {
        let mut tracker = DiscoveryTracker::new();
        let discovery = tracker
            .discover("Fix bug in auth", "task-1", DiscoveryPriority::MustAddress)
            .unwrap()
            .clone();

        assert_eq!(tracker.all().len(), 1);
        assert_eq!(discovery.description, "Fix bug in auth");
        assert_eq!(discovery.source_task, "task-1");
        assert!(!discovery.addressed);
        assert!(discovery.is_blocking());
    }

    #[test]
    fn discover_generates_unique_ids() {
        let mut tracker = DiscoveryTracker::new();
        let d1 = tracker
            .discover("First", "task-1", DiscoveryPriority::ShouldAddress)
            .unwrap()
            .clone();
        let d2 = tracker
            .discover("Second", "task-1", DiscoveryPriority::ShouldAddress)
            .unwrap()
            .clone();

        assert_ne!(d1.id, d2.id);
        assert!(d1.id.starts_with("disc-"));
        assert!(d2.id.starts_with("disc-"));
    }

    #[test]
    fn discover_enforces_must_address_budget() {
        let budget = ScopeBudget::new(10, 2);
        let mut tracker = DiscoveryTracker::with_budget(budget);

        tracker
            .discover("First", "task-1", DiscoveryPriority::MustAddress)
            .unwrap();
        tracker
            .discover("Second", "task-1", DiscoveryPriority::MustAddress)
            .unwrap();

        // Third must-address should exceed budget
        let result = tracker.discover("Third", "task-1", DiscoveryPriority::MustAddress);
        assert!(result.is_err());

        let err = result.unwrap_err();
        let err_msg = err.to_string();
        assert!(err_msg.contains("would exceed budget"));
        assert!(err_msg.contains("must-address"));
    }

    #[test]
    fn discover_enforces_total_budget() {
        let budget = ScopeBudget::new(3, 10);
        let mut tracker = DiscoveryTracker::with_budget(budget);

        tracker
            .discover("First", "task-1", DiscoveryPriority::NiceToHave)
            .unwrap();
        tracker
            .discover("Second", "task-1", DiscoveryPriority::NiceToHave)
            .unwrap();
        tracker
            .discover("Third", "task-1", DiscoveryPriority::NiceToHave)
            .unwrap();

        // Fourth item should exceed total budget
        let result = tracker.discover("Fourth", "task-1", DiscoveryPriority::NiceToHave);
        assert!(result.is_err());

        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("would exceed budget"));
        assert!(err_msg.contains("3 total discoveries"));
    }

    #[test]
    fn address_marks_as_addressed() {
        let mut tracker = DiscoveryTracker::new();
        let discovery = tracker
            .discover("Fix bug", "task-1", DiscoveryPriority::MustAddress)
            .unwrap();
        let id = discovery.id.clone();

        assert!(!discovery.addressed);
        assert!(discovery.is_blocking());

        let marked = tracker.address(&id);
        assert!(marked);

        let addressed = tracker.get(&id).unwrap();
        assert!(addressed.addressed);
        assert!(!addressed.is_blocking());
    }

    #[test]
    fn address_unknown_id_returns_false() {
        let mut tracker = DiscoveryTracker::new();
        assert!(!tracker.address("unknown-id"));
    }

    #[test]
    fn get_returns_discovery_by_id() {
        let mut tracker = DiscoveryTracker::new();
        let discovery = tracker
            .discover("Fix bug", "task-1", DiscoveryPriority::ShouldAddress)
            .unwrap();
        let id = discovery.id.clone();

        let found = tracker.get(&id);
        assert!(found.is_some());
        assert_eq!(found.unwrap().description, "Fix bug");

        assert!(tracker.get("unknown").is_none());
    }

    #[test]
    fn unaddressed_excludes_addressed() {
        let mut tracker = DiscoveryTracker::new();

        tracker
            .discover("Must 1", "task-1", DiscoveryPriority::MustAddress)
            .unwrap();
        let d2_id = tracker
            .discover("Should 1", "task-1", DiscoveryPriority::ShouldAddress)
            .unwrap()
            .id
            .clone();
        tracker
            .discover("Must 2", "task-1", DiscoveryPriority::MustAddress)
            .unwrap();

        tracker.address(&d2_id);

        let unaddressed = tracker.unaddressed();
        assert_eq!(unaddressed.len(), 2);
    }

    #[test]
    fn unaddressed_sorted_by_priority() {
        let mut tracker = DiscoveryTracker::new();

        tracker
            .discover("Nice", "task-1", DiscoveryPriority::NiceToHave)
            .unwrap();
        tracker
            .discover("Should", "task-1", DiscoveryPriority::ShouldAddress)
            .unwrap();
        tracker
            .discover("Must", "task-1", DiscoveryPriority::MustAddress)
            .unwrap();

        let unaddressed = tracker.unaddressed();
        assert_eq!(unaddressed.len(), 3);

        // Should be sorted: Must, Should, Nice (by weight descending)
        assert_eq!(unaddressed[0].priority, DiscoveryPriority::MustAddress);
        assert_eq!(unaddressed[1].priority, DiscoveryPriority::ShouldAddress);
        assert_eq!(unaddressed[2].priority, DiscoveryPriority::NiceToHave);
    }

    #[test]
    fn by_priority_filters() {
        let mut tracker = DiscoveryTracker::new();

        tracker
            .discover("M1", "task-1", DiscoveryPriority::MustAddress)
            .unwrap();
        tracker
            .discover("S1", "task-1", DiscoveryPriority::ShouldAddress)
            .unwrap();
        tracker
            .discover("M2", "task-1", DiscoveryPriority::MustAddress)
            .unwrap();
        tracker
            .discover("N1", "task-1", DiscoveryPriority::NiceToHave)
            .unwrap();

        assert_eq!(tracker.by_priority(DiscoveryPriority::MustAddress).len(), 2);
        assert_eq!(
            tracker.by_priority(DiscoveryPriority::ShouldAddress).len(),
            1
        );
        assert_eq!(tracker.by_priority(DiscoveryPriority::NiceToHave).len(), 1);
    }

    #[test]
    fn must_address_count() {
        let mut tracker = DiscoveryTracker::new();

        assert_eq!(tracker.must_address_count(), 0);

        tracker
            .discover("M1", "task-1", DiscoveryPriority::MustAddress)
            .unwrap();
        tracker
            .discover("S1", "task-1", DiscoveryPriority::ShouldAddress)
            .unwrap();
        tracker
            .discover("M2", "task-1", DiscoveryPriority::MustAddress)
            .unwrap();

        assert_eq!(tracker.must_address_count(), 2);
    }

    #[test]
    fn is_over_budget_with_limits() {
        let budget = ScopeBudget::new(2, 1);
        let mut tracker = DiscoveryTracker::with_budget(budget);

        assert!(!tracker.is_over_budget());

        tracker
            .discover("M1", "task-1", DiscoveryPriority::MustAddress)
            .unwrap();
        tracker
            .discover("S1", "task-1", DiscoveryPriority::ShouldAddress)
            .unwrap();

        assert!(!tracker.is_over_budget());

        // Adding over the limit via direct manipulation (simulating the check)
        tracker.discoveries.push(Discovery::new(
            "disc-3".to_string(),
            "Extra".to_string(),
            "task-1".to_string(),
            DiscoveryPriority::NiceToHave,
            1000,
        ));

        assert!(tracker.is_over_budget());
    }

    #[test]
    fn is_over_budget_no_limits() {
        let mut tracker = DiscoveryTracker::new();

        for i in 0..100 {
            tracker
                .discover(
                    &format!("Item {}", i),
                    "task-1",
                    DiscoveryPriority::NiceToHave,
                )
                .unwrap();
        }

        // No budget set, never over budget
        assert!(!tracker.is_over_budget());
    }

    #[test]
    fn summary_counts() {
        let mut tracker = DiscoveryTracker::new();

        let m1_id = tracker
            .discover("M1", "task-1", DiscoveryPriority::MustAddress)
            .unwrap()
            .id
            .clone();
        let s1_id = tracker
            .discover("S1", "task-1", DiscoveryPriority::ShouldAddress)
            .unwrap()
            .id
            .clone();
        tracker
            .discover("M2", "task-1", DiscoveryPriority::MustAddress)
            .unwrap();
        tracker
            .discover("N1", "task-1", DiscoveryPriority::NiceToHave)
            .unwrap();

        tracker.address(&m1_id);
        tracker.address(&s1_id);

        let summary = tracker.summary();

        assert_eq!(summary.total, 4);
        assert_eq!(summary.unaddressed, 2); // M2 and N1
        assert_eq!(summary.must_address_total, 2);
        assert_eq!(summary.must_address_unaddressed, 1); // Only M2
        assert_eq!(summary.should_address_total, 1);
        assert_eq!(summary.should_address_unaddressed, 0); // S1 addressed
        assert_eq!(summary.nice_to_have_total, 1);
        assert_eq!(summary.nice_to_have_unaddressed, 1);
    }

    #[test]
    fn summary_has_blocking() {
        let mut tracker = DiscoveryTracker::new();

        tracker
            .discover("M1", "task-1", DiscoveryPriority::MustAddress)
            .unwrap();
        tracker
            .discover("S1", "task-1", DiscoveryPriority::ShouldAddress)
            .unwrap();

        let summary = tracker.summary();
        assert!(summary.has_blocking());
    }

    #[test]
    fn summary_total_unaddressed() {
        let mut tracker = DiscoveryTracker::new();

        tracker
            .discover("M1", "task-1", DiscoveryPriority::MustAddress)
            .unwrap();
        tracker
            .discover("S1", "task-1", DiscoveryPriority::ShouldAddress)
            .unwrap();
        tracker
            .discover("N1", "task-1", DiscoveryPriority::NiceToHave)
            .unwrap();

        let summary = tracker.summary();
        assert_eq!(summary.total_unaddressed(), 3);
    }

    #[test]
    fn clear_removes_all_discoveries() {
        let mut tracker = DiscoveryTracker::new();

        tracker
            .discover("Item 1", "task-1", DiscoveryPriority::NiceToHave)
            .unwrap();
        tracker
            .discover("Item 2", "task-1", DiscoveryPriority::NiceToHave)
            .unwrap();

        assert_eq!(tracker.all().len(), 2);

        tracker.clear();

        assert!(tracker.all().is_empty());
    }

    #[test]
    fn clear_resets_id_counter() {
        let mut tracker = DiscoveryTracker::new();

        let _d1 = tracker
            .discover("First", "task-1", DiscoveryPriority::NiceToHave)
            .unwrap();
        tracker.clear();

        let d2 = tracker
            .discover("Second", "task-1", DiscoveryPriority::NiceToHave)
            .unwrap();

        // IDs should restart from disc-1 after clear
        assert_eq!(d2.id, "disc-1");
    }

    #[test]
    fn prune_addressed_before() {
        let mut tracker = DiscoveryTracker::new();

        // Create discoveries with specific timestamps
        let mut discovery1 = Discovery::new(
            "disc-1".to_string(),
            "Old addressed".to_string(),
            "task-1".to_string(),
            DiscoveryPriority::NiceToHave,
            1000,
        );
        discovery1.addressed = true;

        let mut discovery2 = Discovery::new(
            "disc-2".to_string(),
            "New unaddressed".to_string(),
            "task-1".to_string(),
            DiscoveryPriority::NiceToHave,
            2000,
        );
        discovery2.addressed = false;

        let mut discovery3 = Discovery::new(
            "disc-3".to_string(),
            "Recent addressed".to_string(),
            "task-1".to_string(),
            DiscoveryPriority::NiceToHave,
            3000,
        );
        discovery3.addressed = true;

        tracker.discoveries.push(discovery1);
        tracker.discoveries.push(discovery2);
        tracker.discoveries.push(discovery3);

        // Prune addressed items before 2500ms
        let pruned = tracker.prune_addressed_before(2500);

        assert_eq!(pruned, 1); // Only discovery1 should be pruned
        assert_eq!(tracker.all().len(), 2);
        assert_eq!(tracker.all()[0].description, "New unaddressed");
        assert_eq!(tracker.all()[1].description, "Recent addressed");
    }

    #[test]
    fn scope_budget_defaults() {
        let budget = ScopeBudget::default();
        assert_eq!(budget.max_discoveries, 10);
        assert_eq!(budget.max_must_address, 3);
    }

    #[test]
    fn scope_budget_presets() {
        let restrictive = ScopeBudget::restrictive();
        assert_eq!(restrictive.max_discoveries, 5);
        assert_eq!(restrictive.max_must_address, 1);

        let permissive = ScopeBudget::permissive();
        assert_eq!(permissive.max_discoveries, 20);
        assert_eq!(permissive.max_must_address, 5);
    }

    #[test]
    fn set_and_clear_budget() {
        let mut tracker = DiscoveryTracker::new();

        assert!(!tracker.is_over_budget());

        tracker.set_budget(ScopeBudget::new(1, 1));
        tracker
            .discover("Item 1", "task-1", DiscoveryPriority::NiceToHave)
            .unwrap();

        assert!(!tracker.is_over_budget());

        // Add another item via direct manipulation
        tracker.discoveries.push(Discovery::new(
            "disc-2".to_string(),
            "Extra".to_string(),
            "task-1".to_string(),
            DiscoveryPriority::NiceToHave,
            1000,
        ));

        assert!(tracker.is_over_budget());

        tracker.clear_budget();
        assert!(!tracker.is_over_budget());
    }

    #[test]
    fn must_address_unaddressed_count() {
        let mut tracker = DiscoveryTracker::new();

        assert_eq!(tracker.must_address_unaddressed_count(), 0);

        let m1_id = tracker
            .discover("M1", "task-1", DiscoveryPriority::MustAddress)
            .unwrap()
            .id
            .clone();
        tracker
            .discover("M2", "task-1", DiscoveryPriority::MustAddress)
            .unwrap();

        assert_eq!(tracker.must_address_unaddressed_count(), 2);

        tracker.address(&m1_id);
        assert_eq!(tracker.must_address_unaddressed_count(), 1);
    }
}
