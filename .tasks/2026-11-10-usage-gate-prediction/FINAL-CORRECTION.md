Historical record. The current contract is docs/usage-gate.md. Later corrections are recorded in .tasks/2026-11-12-usage-gate-recovery/CAUSE.md. Earlier verification claims apply only to the revisions named below.

# Authorized focused correction

The user authorized one further correction and gate attempt after the c3bd0fa verdict. The current base of this correction is fd494ef, including the separately tested subscription fix.

Root cause:
1. Mechanism: the gate found that pool_burn reads the active alias's per-name history even when another alias performed every measurement. It also identified a pre-read clock race in cached_levels.
2. Why possible: quota deduplication changed the reader's profile name without moving its history writer to the same identity. Freshness was compared with a clock captured before active-identity resolution.
3. Why unnoticed: the alias test called an already sorted helper and did not assert burn. Pending-ping tests asserted absence of model calls without proving readiness was evaluated.
4. Twins: history migration, active-first ordering, hook-time identity lookup, reserve settings, ping-error repetition and the matching README rules are included in this correction.

Finding mapping: 19/27 are history regression; 20/31/32 are documentation siblings; 21/29 are shared-setting duplication; 22/25/33 are repeated-error siblings; 23 is hook subprocess latency; 24 is a stale-clock race; 26/30 are active-order test coverage; 28 is readiness-test observability. Generated hook findings map to these same classes.

Boundaries: history uses quota identity and retains legacy alias samples when no canonical history exists. Missing samples remain unknown, and aliases cannot count twice. Hook paths with automatic switching use the saved selection hint without launching auth status; other paths retain explicit active lookup. Future measurements remain rejected by read_cached_usage, while a legitimate measurement written after an earlier caller clock is accepted. Prediction reserve, lead, limits and horizon have one definition. Ping errors log only when they change. Jobs release pending state on a later valid measurement but credit only the original window, with bounded expiry and the existing rerun gap.

Verification: the full suite passed 116 library and 110 CLI tests after the correction. The final focused alias test additionally covers a malformed optional selection hint without launching an auth binary; the pending-ping test asserts both active and idle readiness logs. The alias fixture sorts the fetching alias before the active profile, checks one shared burn rate, checks legacy migration, and accepts a sample measured after the caller's earlier clock. Formatting, Clippy with warnings denied and diff checks pass. Cargo's remote multiplexing warning was followed by successful local wrapper runs; no build infrastructure was changed.
