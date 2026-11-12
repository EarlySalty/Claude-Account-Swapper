# Session quota pause

The default pause threshold is 97% of the five-hour session allowance. With automatic switching disabled, the active account triggers the pause even when another saved account has quota. A reset due within an hour does not postpone that active-account pause.

Both `PostToolUse` and `Stop` evaluate the shared local quota cache and can activate the pause before the next watcher tick. The hooks make no network calls and do not start `claude auth status`. They bind the active profile to the live refresh-token fingerprint. A missing identity does not count as a measured 97% reading. Existing manual continuation handling remains in place.

During a pause, the watcher compacts eligible open Claude threads one at a time. It waits for an outstanding compaction to finish before sending another. The normal eight-thread total and 60,000-token minimum apply to proactive compaction, not to draining paused sessions. Recent idle threads are eligible immediately after their turn ends; older threads must belong to a mapped paused session. Busy threads and settled threads are excluded. The configured session scope still applies.

Quota remains necessary for summaries. If the account is already exhausted, summaries wait for quota. This covers T3-managed Claude threads; it does not issue `/compact` into independent terminal sessions.

## Resume in the original T3 chat

After quota recovers, sessions stopped by the quota hook resume through T3's chat dispatcher in their existing thread. The swapper sends a continuation with the saved TODO pointer using `orchestration.dispatchCommand`, `message.dispatch`, and `start_immediately`. T3 owns the provider session resume, model, and workspace. The swapper does not create a replacement chat or start a separate CLI resume job for a recognized T3 session.

The swapper waits for the stopped turn to finish and for the context to be ready. A persisted message ID prevents duplicate continuation messages after an uncertain response. If the user has already continued the chat, automatic continuation is skipped. A T3 connection failure leaves the session pending for retry; it does not select the independent terminal-session job path.

## Measurement timing and T3 Code

The swapper still measures the active account at most once every three minutes and idle accounts at most once every ten minutes. Consumers share an identity-bound cache and HTTP 429 cooldown. A hook cannot observe consumption newer than that cache. A 97% threshold therefore reserves headroom but cannot guarantee that a burst never reaches 100% between measurements.

The inspected T3 checkout at `/home/nathanael/repos/t3code-multi-agent` has two sources:

* `apps/server/src/provider/Layers/ClaudeProvider.ts`, `probeClaudeCapabilities`: calls the SDK's `usage_EXPERIMENTAL_MAY_CHANGE_DO_NOT_RELY_ON_THIS_API_YET()` method, which returns the `get_usage` snapshot.
* `apps/server/src/provider/Layers/ClaudeAdapter.ts`: receives `rate_limit_event` messages during turns without a separate polling request. `claudeUsageLimits.ts` converts their 0-to-1 utilization fractions into percentages.

`ProviderUsageLimitsIngestion.ts` folds events into each provider instance's published snapshot. The `server.getConfig` RPC returns those cached providers and their `usageLimits`, `checkedAt`, and authentication metadata. `server.refreshProviders` causes a refresh and should not be used as a quota reader.

T3 and the swapper still have separate measurement caches. This change does not disable T3 probes or claim to deduplicate their requests. A shared reader should consume the existing T3 snapshot or export its live events, preserve each window's measurement time, and bind them to the account and organization across switches. T3's published Claude authentication metadata supplies email but does not establish the swapper's organization identity, so copying a snapshot by provider name alone can assign old or foreign quota to the active login.
