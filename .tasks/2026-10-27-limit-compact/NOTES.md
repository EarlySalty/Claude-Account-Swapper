# Limit-aware pause and compaction

Goal: when every saved account is at the 5h limit, running Claude sessions wrap up into their TODO.md, warm idle T3 threads are compacted while their prompt cache is still warm, and every stopped session is resumed after the reset.

## How it works

- `watch` evaluates the pause every 60s after the usage refresh (`limit::App::limit_tick`).
  - Enter: `limit_compact.enabled` and (`force` or every profile's cached 5h utilization >= `threshold`).
  - Leave: `force` off and usable quota: the active account (or, with auto switch on, any account) has fresh cached 5h usage below `resume_below` and weekly usage below the switch threshold. Disabling the feature also leaves; parked sessions are then dropped with one log line each.
  - On leave the hooks stop telling sessions (`active=false`), but Stops of already parked sessions are still recorded. Only one watcher runs a tick at a time (`.limit-tick.lock`).
- Hook `claude-account hook` (wired in `~/.claude/settings.json` for `PostToolUse` matcher `*` and `Stop`):
  - Inactive pause: reads one small JSON file and exits silently.
  - `PostToolUse`: first call per session injects `additionalContext` with the wrap-up instruction (write TODO.md, end the turn, last line `TODO: <path>`). No tool is interrupted; the agent ends at its own pace.
  - `Stop` of a session that was never told: blocks once with the same instruction. `stop_hook_active` is honoured.
  - Subagents (`agent_id` set) and sessions inside our own `/compact` turns are left alone.
- Compaction pass (pause active): T3 shell over HTTP, then projection and `message.dispatch` with exactly `/compact` over the T3 WebSocket RPC. Only threads that are idle, finished between `idle_minutes` and `cache_ttl_minutes` ago, have at least `min_context_tokens` in the last provider turn and whose last item is not already a compaction. At most `max_compactions` per pause. Busy threads (active run, busy status, pending runtime request) are never touched.
- Resume pass (pause released): each parked session gets a self-contained message pointing at its TODO.md (path from the latest reply written after the wrap-up instruction). Threads the user already continued (also before a compaction) are skipped and logged; busy threads wait. T3 sessions (`CLAUDE_CODE_ENTRYPOINT=sdk-ts`) only resume through their thread; T3 being down keeps them pending. Terminal sessions become a `JobKind::Resume` job that may run in the current window and keeps permission checks, unless their transcript moved on after the stop. Each session's outcome is stored on its own, errors stay pending and are retried. Sessions without a thread or without a seen turn end are given up 30 min after the release with a log line. Every outcome is logged, so nothing stays stopped silently.
- The web panel's config save keeps `limit_compact` as stored; only the CLI changes it.

## State

`~/.claude-account-switcher/limit-compact.json` (0600, flock `.limit-compact.lock`). The switcher store already holds config, usage cache and jobs; the state is a handful of ids and timestamps that only matter during one pause and are reset once every parked session is handled. No database, no SQLite.

T3 access: own token `~/.claude-account-switcher/t3-bearer` (0600, scopes orchestration:read/operate, 30d), re-issued automatically via `t3 auth session issue` on 401/403. Never logged.

## Config knobs (`config.json` -> `limit_compact`, CLI `claude-account limit-compact`)

| Key | Default | Meaning |
|---|---|---|
| enabled | false (live: true) | feature switch |
| force | false | test mode: pause regardless of usage |
| threshold | 95 | all accounts >= this 5h % enters the pause |
| resume_below | 80 | usable account below this releases it (must be > 0 and <= threshold) |
| min_context_tokens | 60000 | smaller threads are not compacted |
| cache_ttl_minutes | 55 | only compact if the last run ended within this (T3 reports a 1h prompt cache TTL) |
| idle_minutes | 2 | do not compact a thread that just finished |
| max_compactions | 8 | cap per pause, compaction costs quota on a nearly full account |
| scope_sessions | [] | if set, only these Claude session ids are affected (used for the test) |
| t3_url | http://127.0.0.1:3773 | T3 server |

## Pre-switch compaction: not implemented

- The auto switch fires when the active account reaches 98%. At that moment the old account has about 2% left; compacting N threads on it would push it into the hard limit and delay the switch that running sessions need right now.
- Running threads cannot be compacted, and those are the ones that continue on the new account.
- Gain only exists for idle threads that are resumed within the 1h TTL. Rough numbers for a 130k context (the test thread): uncached resume on the new account writes about 130k cache tokens; compacting costs about 130k cache read plus a summary turn on the old, full account, then about 14k on the new one. Spending the scarce quota of a 98% account to save quota on a fresh one is the wrong direction. The limit pause covers the case where it does pay off: all accounts are full, so there is nothing to switch to and the wait outlasts the cache.
- Switch frequency: the journal only reaches back to today 22:06 and shows no switch; `last-switch` is 2026-10-27 22:06. No evidence of frequent switches.

## Acceptance proof (2026-10-27, throwaway thread fc22e24b-725e-4e3f-98ba-2ba70b1e166a, opus 5.5 medium, project e18d010f)

1. Thread created via T3 `thread.create`; first turn read a file (68k tokens). Scope set to its session ad107ace-ac44-4608-bae3-6540ad2419a7, force on, pause began 22:33:00.
2. Dummy turn (read six files one by one): after the first Read the hook injected the instruction; the agent wrote `/tmp/cas-test/.tasks/2026-10-27-dummy/TODO.md`, ended its turn with `TODO: /tmp/cas-test/.tasks/2026-10-27-dummy/TODO.md`. Stop recorded, thread mapped.
3. 22:37:28 `Thread fc22e24b... kompaktiert (130289 Tokens im Kontext)`; timeline: `user_message` 21:37:28Z, `compaction completed "Context compacted"` 21:37:49Z; next provider turn 13593 tokens. No follow-up turn from the Stop hook.
4. Force off, 22:38:30 resume message sent: "[claude-account] The 5-hour usage limit has reset ... Read /tmp/cas-test/.tasks/2026-10-27-dummy/TODO.md first ..."; the thread finished the task (2715 lines) and marked the TODO done. `Limit-Pause beendet`.

Gate round 1 found 16 state-machine defects in resume and release handling (see commit); all fixed with regression tests.

Bugs found in the test and fixed: hook logged to stdout (corrupts the hook JSON); TODO path is not yet in the transcript when Stop fires, so it is re-read at resume time.

## Risks

- T3 removed `POST /api/orchestration/dispatch` (404); `t3-harness send`, `t3-wakeup` and `compact-after-subagent.py` still use it and are broken. The swapper uses the WebSocket RPC.
- `~/.config/claude-t3/bearer` is expired.
- Compaction runs on the active account at >= 95%; if it hits 100% mid-compaction the turn fails (logged, not retried).
- Non-T3 resume via `claude --resume` jobs is not exercised in the live test.
- T3 protocol is internal; a T3 update can break shell/projection/dispatch shapes. Failures are logged once per state and leave threads untouched.
