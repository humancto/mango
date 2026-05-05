# Changelog

All notable, user-visible changes land here. The repo is pre-1.0 — no
SemVer guarantees yet, but breaking changes are called out so the
ROADMAP.md item that drove the change is traceable.

## Unreleased

### Changed

- **`mango-mvcc`**: `WatchableStore::watch(range, start_rev)` now
  accepts `start_rev <= current_revision()` and registers the watcher
  into a new **unsynced** group. A per-watcher catch-up driver scans
  history from `start_rev` forward and promotes the watcher to the
  synced group once it reaches the published revision. Reads at a
  revision strictly below the compacted floor still return
  `MvccError::Compacted` (etcd parity). (ROADMAP.md:863)

### Removed

- **`mango-mvcc`**: `MvccError::Unsupported` and the
  `UnsupportedFeature` enum (incl. `UnsupportedFeature::UnsyncedWatcher`)
  are removed from the public API. The variant only existed to reject
  catch-up watch requests; with the path now implemented, the variant
  has no remaining surface. The `MvccError` enum is `#[non_exhaustive]`
  so external `_` match arms continue to compile. (ROADMAP.md:863)
