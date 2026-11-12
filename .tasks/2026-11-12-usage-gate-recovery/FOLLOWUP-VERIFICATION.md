# Acceptance follow-up

Commit 505a708 preserves explicit manual runs of disabled jobs while scheduled execution still reloads and rejects disabled state. Corrupt canonical gates are copied aside and replaced atomically, keeping the canonical path present for concurrent readers.

The added CLI regression explicitly disables a job, runs it manually twice, verifies that each execution reaches the command, and verifies that the scheduler flag remains disabled. cargo-slot test --locked --test cli manual_runs_execute_disabled_jobs_without_enabling_the_scheduler: exit 0, one passed. The continuation handover for 505a708 records 117 library and 112 CLI tests passing; this targeted test adds coverage to that result. cargo-slot clippy --all-targets -- -D warnings, cargo fmt --check, node --check web/app.js and git diff --check: exit 0.

The earlier gate for c7d769f ended without a verdict. Its output log is empty, its process no longer exists, and its persisted job metadata still says running with the departed PID. It does not count as round four. The next verdict is round four. No integration or deployment has occurred.

Cause: the earlier fixture only executed enabled jobs, so it did not distinguish a scheduler safety check from explicit manual execution. The new fixture checks the public CLI behavior and actual child invocation independently of internal branching.
