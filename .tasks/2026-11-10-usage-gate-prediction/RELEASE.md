# Shared usage gate and quota protection

The release consolidates the implementation and two review correction rounds. Earlier branch commits remain pushed under fix/usage-gate-prediction. The independent review and gate logs remain in the local task directory; the final release commit is gated separately before deployment.

Root cause:
1. Mechanism: the supplied November 10 service journal showed repeated HTTP 429 responses and stale 84% readings until a fresh 100% reading finally triggered switching.
2. Why possible: watcher, web, CLI and post-job readers did not share a single cache/request reservation. Token-keyed cooldowns could be reset by rotation. See App::profile_usage and its callers in src/lib.rs.
3. Why unnoticed: direct readers re-dated cached measurements, reserve protection needed history, and projection was not visible. The final readers preserve measurement time and expose age, forecast pressure and cooldowns.
4. Twins: active and idle pings, job confirmation, weekly pacing, aliases and organization identity, CLI/web age readers and both reset windows now use the shared boundaries. Discretionary weekly pacing retains its dwell; safety reserve checks work immediately after an automatic switch.

Verification: cargo-slot test --quiet passed 111 library tests and 106 CLI tests after the final dwell fix. The cold-history CLI regression writes a recent last-switch timestamp and still requires a reserve-driven switch. Application tests use mock endpoints and temporary stores. No production account was changed for these tests.

State boundaries: missing gate starts cold; malformed gate fails with an error; future measurements are not fresh; a missing previous sample permits measured reserve protection without a slope. Quota identity includes normalized email and organization, with unknown organization distinct from a known one. Pending job and ping confirmation requires a later measurement from the run quota and has a bounded timeout. HTTP 429 cooldown survives restarts and token rotation. Reasonable Retry-After values through 24 hours extend the cooldown; larger values are logged as invalid and use bounded local backoff.

Limitations: coordination covers processes sharing the same local store. Passive confirmed quota events are unavailable in the current hook schema. Sudden usage jumps can still exhaust an account between measurements. Existing manual pins for soft limits remain until actual exhaustion at 100%; hard limits release the pin at their configured threshold. Prediction does not override these pins. Deployment and live evidence follow the final gate.

## Correction after release-gate BLOCK

The gate against e5e74bb identified correctness gaps in deferred confirmations, ping readiness, target reserves and pooled aliases. Confirmation now checks expiry before evidence, and accepts only a later measurement whose reset could belong to the original five-hour window. Pending pings reserve their quota before sending, block aliases and expire without requiring a successful measurement. Invalid ping state is logged and blocks pings while quota switching continues. Weekly pacing, prediction and reactive preference share one target reserve check. Limit-pause levels deduplicate quota identities with the active profile first. The stale marker includes the watcher tick and loop margin.

Boundary inventory: confirmation timestamps that are future, expired or equal to the measurement cannot prove a run. A reset outside the original window is not credited to that run. Empty pending maps require no state write; missing state starts empty; malformed or unreadable state rejects pings without stopping switching. All ping claims and confirmations use the ping lock. Missing/deleted quota identities expire after five hours. Old per-name idle timestamps remain readable; new timestamps use quota identity. Aliases retain one active profile's limits in pooled calculations; different organizations remain separate. Target reserve checks preserve reactive last-resort recovery when no safe destination exists.

Regression evidence: the corrected fixtures use realistic five-hour reset times. Pure tests reject later-window and expired confirmation, and test identity-shared pending holds. CLI tests exercise pending active/idle pings under 429, reserve-aware weekly pacing and quota switching with a broken ping-state path. Alias-level tests verify measured active limits and exclusion of a same-quota target.

Final correction validation on November 11: cargo-slot test --quiet passed 114 library tests and 109 CLI tests. cargo-slot clippy --quiet --all-targets -- -D warnings, cargo fmt --check and git diff --check passed. Gate outcome and live deployment remain pending.
