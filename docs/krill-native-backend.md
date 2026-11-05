# Krill native Codex backend

This fork adapts the native/synthetic split, response-frame routing, compaction
ownership and HTTP zstd fallback approach from
[@krill-lab/Superba](https://github.com/krill-lab/superba), revision
`f78d77a2849aceb8dffc1f27a83869ffceab7e90`. Thank you to Krill and the Superba
contributors. The original MIT license is included in
[`licenses/Superba-MIT.txt`](../licenses/Superba-MIT.txt).

The implementation uses CC Switch's own provider selection, authentication,
HTTP clients and usage logging. Krill is identified by an exact,
case-insensitive provider name match (`krill`, `Krill` or `KRILL`), independent
of the API domain. Native compaction defaults apply to GPT models on these
providers; other providers keep the existing summary bridge. Backend metadata
overrides `codexNativeCompaction` and
`codexRequestZstd` are independent. No new UI controls are introduced here.

Native compaction items are forwarded as issued by the upstream. Existing CCS
summary envelopes still expand into text for backward compatibility. Only
fingerprints, provider identifiers and credential fingerprints are stored in
the device-local `codex-compaction-owners.json`; no ciphertext or tokens are
stored. A model explicitly selected from another provider is refused when it
conflicts with a known compaction source. Unlike failover routing, aggregation
must not silently change the selected provider. Unknown imported history is
tried intact and adopted only after a successful native response. A native
compaction rejection does not erase the only copy of earlier context.

HTTP `/responses` uploads above 1 KiB are zstd-compressed when enabled and when
compression reduces size. Large bodies compress outside the async worker.
An immediate 400/415/422 retries the same provider once with plain JSON. A
415 or a different plain response records refusal for 24 hours, scoped to the
provider, endpoint and credential. Chat/Anthropic conversion paths are unchanged.

WebSockets select the provider from `response.create`. A connection never moves
an incremental conversation to a different provider. Switching providers closes
with 1012; unsupported WS routes use the existing HTTP fallback. No upstream
per-message deflate is negotiated. Context compaction and HTTP zstd compression
are separate features.

Upstream Upgrade handshakes use an HTTP/1.1-only client with the existing proxy
policy. They do not negotiate HTTP/2 or follow redirects. One transport failure
may retry the handshake before any generation frame is sent; generation itself
is never automatically replayed by this retry.

TCP/TLS connection attempts retain CC Switch's 30-second connection timeout.
The Upgrade request uses the existing 600-second client limit rather than a
separate 30-second total handshake deadline. Established WS generations have
no added response deadline. Stopping the proxy cancels a pending handshake.
Live acceptance waits up to 600 seconds per response and records elapsed time;
reaching that test limit does not establish that a protocol is unsupported.
Superba separately uses a 4-second connect timeout with no first-byte deadline.

The WebSocket `OpenAI-Beta` negotiation header is retained. Native Krill routes
also keep the non-account `originator` and `version` protocol markers. Session
routing identifiers are retained for the native route, matching Superba; these
identify the conversation, not the ChatGPT account. Official
authorization is replaced by the selected third party's key; account,
attestation and foreign turn-state headers remain isolated.

If the upstream WebSocket cannot connect before any generation was sent,
the session reconnects with 1012 and receives 426 on its next handshake to
use the existing HTTP path. A third-party 401 is exposed as 502 on the mixed
aggregation ingress, retaining the upstream error, so it cannot refresh the
official login. Official authentication failures retain their own status.

CC Switch's routing mode with automatic failover stays on its existing HTTP
retry/circuit-breaker path. Aggregation continues to select its explicit member
over WS. Native JSON responses, including the dedicated `/responses/compact`
object, are observed for ownership; encoded JSON is decoded only for observation,
and its original response bytes and headers continue through CCS processing.

This is not a port of Superba's entire router. CCS keeps one configured Krill
endpoint, its own timeout and capacity policies, one-shot per-session HTTP
fallback, and its existing summary-envelope compatibility. Superba's multi-line
selection, active probes and one-hour per-line WS-refusal memory are not added.
