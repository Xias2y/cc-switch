# Codex aggregation capability badges

The aggregation provider cards append **WS** and **CMP** after their existing
default/official/model-count chips. They use the original `CardChipBadge`,
outline border, 18-pixel height, padding, typography and native title tooltip.
Only the text color changes: `text-success-text` for recent confirmed
availability and `text-fg-3` for unavailable, unknown or expired evidence.
No separate status panel, toggle, CTX badge or TOOL badge is added.

## Passive observation only

Starting with **4.0.26-krill.3**, neither badge sends capability-check requests.
There is no synthetic prompt, compaction trigger, background generation, probe
retry or token usage introduced by the badges. The user's normal model requests
and real context compaction retain their usual upstream usage and billing.

The chips describe the provider's most recently used GPT model and the exact
configuration/account scope observed. They do not assert that every model in
the provider card has the same capability. The tooltip identifies the model,
check time and reason. Before evidence is available, both chips are gray.

- **WS** observes normal Upgrade handshakes and successful WS responses.
  Evidence expires after 30 minutes without another verification.
- **CMP** observes successful native compaction from normal conversation traffic,
  including the native encrypted output. Evidence expires after 24 hours.
  If no real compaction has occurred, CMP remains gray as unverified even when
  the provider supports it. Expiry never triggers an active check.

Model use updates the last-used model and reads the local route configuration.
A route configured to disable native compaction or use a summary bridge is
shown unavailable without contacting the provider. One transient WS failure
can retain a success from the last 15 minutes; repeated failures separated by
at least a minute, or a definite refusal, turn the chip gray.

The UI refreshes from a local Tauri command every 30 seconds while visible and
invalidates on capability-change events. Reading badges never sends a network
request. Configuration, API key, endpoint or official account changes invalidate
the corresponding evidence. Cache entries contain configuration/account hashes,
model names and status metadata, not tokens, keys, response bodies or history.

## Release history

The badges were introduced in `4.0.26-krill.2`. That version included a small
isolated CMP probe on model use. **4.0.26-krill.3 removes that probe entirely**.
Previously cached evidence can remain until its normal expiry; it does not
cause new requests. This change does not claim a new live model acceptance run.
