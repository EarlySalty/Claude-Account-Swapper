# Serialized boundaries and readers

UsageGate cached/previous: missing or null means no sample. Future samples are excluded by read_cached_usage and locked fetch; old samples do not establish a trend. Cached values retain fetched_at across CLI, web and scheduler readers. until/failures missing default to zero; null numeric fields reject decoding. Invalid persisted delays above 24 hours are discarded with a log entry. Identity v2 uses normalized email and optional organization; unknown/empty organization is null and cannot equal known organization. Legacy email canonical files are not imported; per-profile legacy measurements remain readable.

Job pending_confirmation: missing/null means no deferred confirmation. Present state requires account and completed_at; incomplete objects reject decoding. Empty/unmatched account or future completion cannot confirm another account. load_jobs resolves only under the jobs lock from a measurement strictly after completed_at for that identity. Completion status and last_window remain intact while waiting. A confirmed running window sets last_window and clears pending; a later empty measurement clears pending only after five hours. readiness blocks pending repeats. Cross-account measurements cannot resolve pending state.

Utilization: nonnegative finite numbers, including over 100, remain measurable; exhaustion is decided by existing threshold readers. Negative/nonfinite input rejects. Retry delays 0/missing use local backoff; 86400 is honored; 86401 and u64 maximum are invalid and logged.

Global ledger writes are outside this worker's explicitly assigned filesystem ownership. Prepared per-finding ledger entries are stored locally for parent import.
