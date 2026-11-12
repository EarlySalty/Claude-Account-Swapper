# Round-three cause analysis

Mechanism: inactive subscription recovery calls only reactive_target, omitting the soft-reserve and earliest-reset stages. Every failed usage attempt persists until and subsequent calls report UsageThrottled, permitting old data reserved for actual HTTP 429 responses.

Why possible: retry suppression and server throttling share one field and error; recovery duplicates only the first stage of the normal selector.

Why unnoticed: the subscription fixture uses a target below every stop; request-failure fixtures do not repeat the decision during the persisted retry delay.

Twins: all profile_usage_within consumers, active and reserve selection, persisted legacy gates, aliases, subscription identity matching, concurrent cache timestamps, ping readiness and job confirmation readers. Retry delay must never authorize stale fallback unless an actual 429 budget exists. Unknown or corrupt persisted state must not manufacture throttling.

Input edges: missing retry_after defaults to zero; old per-quota until is ordinary retry suppression; only store-wide until denotes server throttling. Missing cached usage cannot recover from an error. Unknown organization matches the authenticated profile response for subscription verification. Reserve recovery respects hard limits and excludes the inactive identity.

Self-review: canonical migration happens once under the usage lock before subscription reservation creates the gate. Failed-request reservations use the per-quota retry timer and return an ordinary error; only the persisted store-wide 429 budget yields UsageThrottled. Profile requests use a separate lock and profile budget, shared by all profile consumers. Dashboard config saves preserve the shared prediction settings. Job execution reloads state under the job-state lock before its first write, then releases it before execution; read-only job consumers never take the run lock. Auxiliary confirmation errors leave unrelated jobs readable.

Moli DOM proof: both saved accounts inactive produces a disabled quick-switch button labelled “Kein aktives Abo bestätigt” and explains that no other saved account has a confirmed active subscription. Layout screenshot was rendered and visually inspected at /tmp/cas-recovery-ui/panel.png. Local-file navigation was unsupported; the same local fixture was loaded as a self-contained data URL.
