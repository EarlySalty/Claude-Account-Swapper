Historical implementation record, superseded by FIX-CLASSIFICATION.md and docs/usage-gate.md for current behavior. The original commit labels require parent resolution.

Usage gate and predictive switching

Ursache:
1. Mechanism: supplied Nov 10 journal evidence shows repeated 429 responses and stale 84% until a measured 100% triggered switching. Current src/lib.rs:2185 keys cooldown by access token; :2204 caps Retry-After.
2. Enabling gap: profile_usage fetches without a process lock or cache recheck. web_refresh_usage, CLI usage, ping_idle_profile and active_usage_fresh reach it directly. Both user service units execute ~/.cargo/bin/claude-account without storage overrides.
3. Missing observability: cached values have age but direct callers rewrite fetched_at. Decisions do not report a same-window projected limit crossing.
4. Siblings: watcher cached path, web refresh, CLI usage, idle jobs and post-job checks share the same fetch method. Existing forecast history has only five-hour percentages, without reset identity. Global graph query returned unrelated repositories, so current source is authoritative. No third installation verified.

Input/readers inventory:
Usage percentages: required numeric finite 0..100; absent/null/empty/overflow fail parsing; zero valid. Reset: absent/null means unknown, empty/invalid fails; prediction requires matching known future reset. Timestamps: future/zero/stale do not establish a slope; repeated/zero elapsed and decreasing values rejected. Cache: absent starts cold, invalid fails closed in gate; cache hits retain measured time. Cooldown: missing starts empty; zero Retry-After gets floor; server delays above 24 hours are rejected; repeated 429 grows persisted delay; concurrent callers serialize and recheck. Identity: normalized nonempty email plus organization shared across aliases and token rotation; unknown email fails closed. Readers: watcher, web, CLI, jobs and manual refresh all enter profile_usage. Existing pin and session-limit paths retain their distinct semantics.

Scope: local shared store only. Parent owns independent Rust review, gate, merge and deployment.

Verification: cargo-slot test --quiet passed 111 library tests and 101 CLI integration tests. cargo-slot clippy --all-targets -- -D warnings passed. Cargo 1.99 fmt --check and git diff --check passed. All final tests used mock HTTP or a closed local endpoint. The pre-existing harness omission was repaired after initial runs exposed it; no real credentials were used by the temporary test harness. Production services were neither restarted nor switched.

Defaults: active 180 s, inactive 600 s, HTTP timeout 10 s, reserve 5 percentage points, lead 60 s. Shared 429 exponential 300/600/900 s, valid server Retry-After may extend up to 24 hours. Known limits: shared local store only, normalized email plus the persisted organization ID, no sudden-jump guarantee, no confirmed quota signal in existing HookInput. Parent performs independent Rust review, gate, merge and deploy.
