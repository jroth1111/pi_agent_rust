# Persistence

Persistence-specific knowledge for the mission.

**What belongs here:** Authoritative store decisions, migration/cutover rules, projection guidance, durability invariants.

---

## Mission persistence direction

- One authoritative session event store after cutover
- One authoritative workflow store and transition journal after cutover
- Projections are rebuildable derived state only
- Legacy JSONL/V2/direct-SQLite session paths must become migration/import/export-only after cutover
- Migration/cutover must be atomic, correlation-linked, and fail closed
- Rollback evidence must survive cleanup in a surviving authority or rollback journal
