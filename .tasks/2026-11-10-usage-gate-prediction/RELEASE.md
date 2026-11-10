# Shared usage gate and quota protection

The release consolidates the implementation and two review correction rounds. Earlier branch commits remain pushed under fix/usage-gate-prediction. The independent review and gate logs remain in the local task directory; the final release commit is gated separately before deployment.

Root cause:
1. Mechanism: the supplied November 10 service journal showed repeated HTTP 429 responses and stale 84% readings until a fresh 100% reading finally triggered switching.
2. Why possible: watcher, web, CLI and post-job readers did not share a single cache/request reservation. Token-keyed cooldowns could be reset by rotation. See App::profile_usage and its callers in src/lib.rs.
3. Why unnoticed: direct readers re-dated cached measurements, reserve protection needed history, and projection was not visible. The final readers preserve measurement time and expose age, forecast pressure and cooldowns.
4. Twins: active and idle pings, job confirmation, weekly pacing, aliases and organization identity, CLI/web age readers and both reset windows now use the shared boundaries. Discretionary weekly pacing retains its dwell; safety reserve checks work immediately after an automatic switch.

Verification: cargo-slot test --quiet passed 111 library tests and 106 CLI tests after the final dwell fix. The cold-history CLI regression writes a recent last-switch timestamp and still requires a reserve-driven switch. Application tests use mock endpoints and temporary stores. No production account was changed for these tests.

State boundaries: missing gate starts cold; malformed gate fails with an error; future measurements are not fresh; a missing previous sample permits measured reserve protection without a slope. Quota identity includes normalized email and organization, with unknown organization distinct from a known one. Pending job and ping confirmation requires a later measurement from the run quota and has a bounded timeout. HTTP 429 cooldown survives restarts and token rotation. Reasonable Retry-After values through 24 hours extend the cooldown; larger values are logged as invalid and use bounded local backoff.

Limitations: coordination covers processes sharing the same local store. Passive confirmed quota events are unavailable in the current hook schema. Sudden usage jumps can still exhaust an account between measurements. Explicit manual pins retain their existing precedence until their configured limit is reached. Deployment and live evidence follow the final gate.
