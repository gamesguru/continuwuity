# MSC4499: Server Key Notary Endpoint

This document outlines the additions and clarifications to the MSC4499 specification (Server Key Notary Endpoint) based on the finalization pass.

## 1. Deterministic Pruning and Vanished Keys

The permanent-binding requirement ensures a retained history of keys. However, keys that were once observed in `verify_keys` and then vanished from subsequent responses without appearing in `old_verify_keys` have no `expired_ts`.

To maintain a deterministic sorting order for pruning under the 3,000-key ceiling, every binding is assigned a synthetic ordering timestamp: `effective_expired_ts`.
- If a key is published in `old_verify_keys`, `effective_expired_ts = expired_ts`.
- Otherwise (for vanished keys), `effective_expired_ts` is the local timestamp of the last observation in which the key appeared active.

Because observation times differ across servers, this makes the sort key partially local, slightly weakening the cross-server convergence claim, but ensuring the local database can deterministically sort vanished keys.

## 2. Key ID Tie-Break Collation

When multiple retired keys have the same `effective_expired_ts`, they are pruned based on their `algorithm:key_id`. To ensure determinism, the tie-break relies on a bytewise lexicographic comparison of the full `algorithm:key_id` string as UTF-8. The sort order is ascending by timestamp, and descending by `key_id`, so that lexicographically *smaller* identifiers are retained first (i.e. the largest identifiers are evicted first).

## 3. Eviction Pressure and Permanent-Binding Guarantee

*To be added to Security Considerations:*

Eviction under the 3,000-key ceiling converts a permanent binding back into a TOFU-pending binding. The ceiling bounds collision-blindness protection to the 3,000 most-recently-retired keys. An origin willing to burn its own history can push a target binding out of that window by publishing waves of synthetic retired keys with fresh `expired_ts` values, after which peers will re-TOFU the evicted binding. This bounds the permanent-binding guarantee to the retention window.

## 4. Ceiling on `old_verify_keys`

A single response may contain an arbitrarily large `old_verify_keys` dictionary. To align the wire limit with the storage cap and prevent excessive processing and allocation vectors, a response containing more than 3,000 `old_verify_keys` entries MUST be rejected as malformed.

## 5. `minimum_valid_until_ts` Actor and Override Authority

If a notary query requests a `minimum_valid_until_ts` and the origin serves conflicting key material for the same key ID, the **notary** server MUST reject the new key as a collision. This ensures the freshness-driven re-fetch does not become a collision side-channel that overrides First Seen Wins.

For the client of the notary: a notary response returned in satisfaction of `minimum_valid_until_ts` is still a provisional observation subject to the ordinary rules; freshness confers no override authority.

## 6. Provisional-Binding Freeze

*To be added to Security Considerations:*

An expired or retired provisional binding MUST NOT be overridden by a later direct fetch. This means that a notary-poisoned binding that expires before direct confirmation is frozen forever, and is recoverable only via manual eviction API. This is a deliberate trade-off, preferring the auditability of history over automated healing.

## 7. Immediate Fetch Attempt Amplification

An inbound federation request whose authentication requires a key fetch for a backoff-listed server SHOULD permit at most one probe per backoff interval per remote server, triggered by inbound demand. All other demand within the interval MUST fail fast against the negative cache.

## 8. Notary Ceiling Scope vs Forensic Index

The 3,000-key ceiling governs the notary's *served* binding set (what it returns to peers). In contrast, the forensic index is an implementation-private log outside the ceiling that stores rejected material and collisions (not served bindings).

## 9. Duplicate JSON Key Rejection

The rejection of payloads with duplicate JSON keys applies to the entire response document (any object at any depth). This enforces strict Canonical JSON rules (RFC 8259 permits duplicate members with undefined semantics, which is why parsers silently deduplicate, leading to potential bypasses).

## 10. Implementation and Rollout Notes

*Non-normative.*

During the observation phase, the primary value of the payload is passive monitoring: servers log mismatches without triggering automated remediation, accumulating data on how often divergence occurs across the federation. Automated pipelines can be enabled incrementally once operators have confidence.

## 11. The MSC45XX Equivocation Record

The Equivocation Record is defined adjacent to the Notary endpoint's (`E4`: `/_matrix/key/v2/query`) `409 M_CONFLICT` behavior. The `notary_equivocations` array rides on both the `409` body and, optionally, ordinary query responses when the notary holds relevant records.

The `/_matrix/key/v2/server` (`E1`) direct fetch endpoint mirrors this with a single field pointing to the `E4` definition.

### Critical Cross-MSC Invariant

**Equivocation evidence, including cryptographically verified proofs, is advisory forensic material only; it MUST NOT trigger automated eviction, rebinding, re-verification rollback, or any deviation from First Seen Wins on the consuming server.**

### Wire Format

```json
{
  "notary_equivocations": [
    {
      "record_version": 1,
      "observed_server_name": "example.org",
      "algorithm": "ed25519",
      "short_id": "k1",
      "first": {
        "key_id_sha256": "<unpadded base64>",
        "server_key_package_sha256": "<optional>",
        "first_observed_ts": 1770000000000,
        "observed_via": "direct"
      },
      "conflicting": {
        "key_id_sha256": "...",
        "server_key_package_sha256": "...",
        "first_observed_ts": 1770000600000,
        "observed_via": "notary"
      },
      "first_response": { "...full origin key response...": "optional" },
      "conflicting_response": { "...full origin key response...": "optional" },
      "notary_server_name": "notary.example.net",
      "signatures": { "notary.example.net": { "ed25519:k1": "..." } }
    }
  ]
}
```

### Design Notes

- **Signed Per-Record**: Standard Matrix object signing is applied over the canonical JSON of the record minus `signatures`. This allows records to survive being relayed and cached individually.
- **Provenance**: `observed_via: direct | notary` ties into the two-tier model. Conflicting `direct` observations represent origin equivocation or hijack, whereas `notary` relayed observations may reflect a poisoned upstream notary.
- **Optional Proof Upgrade**: Full embedded responses (`first_response`, `conflicting_response`) are optional. Servers MUST NOT include more than N=10 full-proof records per response; remaining records are hash-referenced. (An entry appearing only in `old_verify_keys` is attested by the origin's *current* signing key, proving "the origin's current key vouched for this historical binding").
- **Timestamps**: Two timestamps (`first_observed_ts`) establish ordering for First Seen Wins semantics. `last_observed_ts` is intentionally omitted to prevent the record from becoming a telemetry stream.
- **Dedup Identity**: A record is identified by `(observed_server_name, algorithm, short_id, first.key_id_sha256, conflicting.key_id_sha256)`, treating the hash pair as unordered. Notaries MUST NOT emit duplicate records for the same identity in one response, and consumers deduplicate on it.
