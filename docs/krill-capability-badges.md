# Codex aggregation capability badges

The aggregation provider cards append **WS** and **CMP** after their existing
default/official/model-count chips. They use the original `CardChipBadge`,
outline border, 18-pixel height, padding, typography and native title tooltip.
Only the text color changes: `text-success-text` for recent confirmed
availability and `text-fg-3` for unavailable, unknown or expired evidence.
No separate status panel, toggle, CTX badge or TOOL badge is added.

## Meaning

The chips describe the provider's most recently used GPT model and the exact
configuration/account scope that was observed. They do not assert that every
model in a provider card has the same capability. The tooltip identifies the
model, check time and reason. Before first use, both chips are gray.

- **WS**: verified by the application's normal Upgrade handshake and subsequent
  successful WS responses. Evidence expires after 30 minutes without another
  verification. No extra generation is sent to check WS.
- **CMP**: native context compaction, separate from HTTP zstd upload compression.
  A real successful native compaction updates the status. Official/Krill GPT
  models with native compaction enabled may also run one small isolated probe
  when first used without fresh evidence. Successful evidence is cached for
  24 hours; no idle timer sends new upstream probes.

The first-use signal is an actual routed GPT request/prewarm, because merely
opening the CCS window does not reveal which model an external Codex client
selected. Model activation checks the cache; continued real use checks it again.
After a temporary failure the next isolated check waits at least 15 minutes and
requires further model use. Definite refusals are cached for 24 hours. Real
protocol successes and explicit refusals update evidence in between.

One transient connection failure can retain a success from the last 15 minutes;
repeated failures separated by at least a minute, or definite protocol/auth
refusal, turn the chip gray. Thus a gray chip can mean "not confirmed" rather
than "this provider never supports the feature"; the tooltip distinguishes it.

## Probe isolation and cost

The CMP probe can consume a small amount of account quota. It sends only a short
synthetic marker and `compaction_trigger`, with `store:false`, no tools and a
separate session. It never copies user history, substitutes a model, replays
generation after failure or overwrites conversation compaction. It uses the
selected provider's existing auth and proxy policy; official quota protection
still applies. Probe usage is logged with a `capability-probe-` session id when
the existing usage log is enabled. The probe waits at most 180 seconds and is
cancelled when the proxy stops. Other providers do not receive automatic probes.

The UI refreshes from a local Tauri command every 30 seconds while visible and
invalidates on capability-change events. Reading badges never sends a network
probe. Configuration, API key, endpoint or official account changes invalidate
the corresponding evidence. Cache entries contain configuration/account hashes,
model names and status metadata, not tokens, keys, response bodies or history.

## Release

Included in `4.0.26-krill.2`, following the backend-only `krill-v4.0.26-1`
release. The badge implementation passed static type/lint checks and a renderer
build. Earlier backend test results do not establish live behavior of the new
capability probes; no new live model acceptance run is claimed for this release.
