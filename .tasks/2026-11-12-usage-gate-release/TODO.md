# Usage gate release: state and next step (2026-11-12)

Branch fix/usage-gate-release in /home/nathanael/.worktrees/cas-usage-integration. Nothing merged, nothing deployed.

Gate counter: 3 counted rounds (verdict logs: /tmp/cas-usage-gate-84b9957.log and earlier). The round-4 run for c7d769f produced no verdict (log empty, process gone), so it is not a round. No gate process is running for this repo.

Commits: c7d769f (round-3 findings fixed) plus a follow-up commit with the two acceptance fixes:
1. execute_job(job, manual): explicit `job run` ignores the enabled flag; the scheduler thread passes manual=false and still rejects disabled jobs after reloading state.
2. recover_gate copies corrupt bytes aside and atomically replaces the file, so the canonical gate path never disappears and legacy fallback is not reactivated.

Verified: clippy --all-targets -D warnings exit 0; 117 lib tests pass; 112 CLI tests pass (two CLI tests failed once in a run concurrent with other builds, passed alone and in a full rerun: load-dependent flake, cause not investigated).

Next step (after the 06:20 Berlin quota reset):
1. `gate-review --repo /home/nathanael/.worktrees/cas-usage-integration --base origin/main` as round 4, log to /tmp/cas-usage-gate-round4.log. Run gate-learning first if it blocks.
2. ALLOW: merge to main, push, deploy the way the repo deploys, then live proof with /tmp/cas-recovery-runtime-proof.py against the installed binary (reserve account picked after a failed request, then recovery). Baseline failure evidence: /tmp/cas-runtime-before-release.log.
3. BLOCK: write GATE-RETRO.md (why four rounds, REAL vs TRIVIA, fix plan), one Fable ask_for_help at medium effort, one clean fixer pass, one final round; if that blocks, stop and report to the user.

Do not touch the stop-hook worktree (517d6bf) or the dirty canonical checkout.
