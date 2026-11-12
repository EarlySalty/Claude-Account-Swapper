# Verification before round four

cargo-slot test --locked: exit 0, 117 library tests and 112 CLI tests. New regressions cover repeated failed requests (401 and 500), stale reserve exclusion throughout the retry delay, successful retry and credential switching, inactive-account soft-reserve recovery, and pending job confirmation expiry. The concurrent CLI fixture now verifies delivered measurements for every consumer. The predictive target fixture uses an account inside its stops but inside the safety reserve.

cargo-slot clippy --all-targets -- -D warnings: exit 0. cargo fmt --check, node --check web/app.js and git diff --check: exit 0.

Moli 1.1.14 DOM and layout proof: both saved accounts lack an active subscription. The quick-switch button is disabled and reads “Kein aktives Abo bestätigt”; the flow explains that no other saved account has a confirmed active subscription. Rendered screenshot: evidence/inactive-accounts.png, visually inspected. A self-contained data URL was used because file URL navigation requires a capability Moli did not grant.

Source review covered changed writers and their readers: retry gates and stale fallback, profile endpoint consumers and persisted verification time, canonical cache migration, shared config and dashboard saves, job-state writers and confirmation reads, ping identity and readiness, CLI documentation and dashboard empty states.

Gate counter before this correction: three counted rounds. No new gate verdict yet. Nothing merged or deployed.
