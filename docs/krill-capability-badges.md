# Codex aggregation WS status

Provider cards show only **WS** after the existing default/official/model-count
chips. It uses the original `CardChipBadge` with green or gray text and no title,
help cursor or tooltip. CMP is not displayed. Context compaction still works.

## Actual traffic only

No handshake, heartbeat, generation or compaction check is sent for this label.
The label's code only observes normal routed requests and responses:

- A successful WS upgrade/send or completed turn records green.
- Received events refresh a long-running turn at most every 30 seconds.
- An HTTP Responses request or WS fallback records gray, even after earlier WS success.
- WS connection/send failures and observed upstream closure record gray immediately.
  Application-level errors carried by a functioning WS do not by themselves change its transport.
- Restarting the application or stopping the proxy clears prior WS evidence.

The visible label reflects the provider's most recently observed GPT-model traffic,
scoped to its configuration and official account. Concurrent sessions may use
different transports; the label does not describe every session at once.
Evidence expires after 30 minutes without further traffic. Gray therefore means
HTTP, interrupted or not recently observed, rather than unsupported forever.

The UI reads local state every 30 seconds while visible and on state-change events.
It never initiates upstream traffic. Passive CMP metadata from earlier versions
may remain in the local cache, but it is not displayed or actively probed.

## History

`0.0.2` added WS / CMP labels and an isolated CMP probe. `0.0.3` removed that probe.
`0.0.4` hides CMP, removes the WS tooltip and uses actual HTTP/WS traffic without
the former WS-success grace period. Backend updater versions retain the existing
`4.0.26-krill.N` sequence through the local 0.0.4 build. Stable release
`CC Switch For Krill 0.1.0` uses `0.1.0` consistently for the app, updater and
release. Its fingerprint excludes hydrated candidate endpoints and sorts JSON
keys so request and provider-list reads match the same transport evidence.
