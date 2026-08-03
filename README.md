# Legal Service Router

Constraint-based routing to accessible public legal-service locations. Hard
capability gates (service category, mandatory accessibility, temporary
closures) are evaluated **before** any cost comparison — a nearer point that
lacks a required capability is excluded, never preferred. Stack: Rust stable,
Axum, SQLite (bundled, WAL). No external map or route API.

## Source material

`materials/service-catalog.json` fixes service-point IDs, capability names, catalog versions, closure events, capability degradation events, and home-service eligibility examples. `POST /catalog/import` accepts exactly this format (`capabilityEvents` is optional in other catalogs).

## Build, test, run

```bash
cargo build
cargo test                 # 22 tests: unit + end-to-end API tests
cargo run -- --import materials/service-catalog.json
```

Env: `ROUTER_DB` (SQLite file, default `./legal-service-router.db`), `PORT` (default `8080`). `--import <file>` imports and activates a catalog at startup; re-importing an existing version is a no-op (409 over HTTP).

## Schema

Created automatically on startup (`db::init_schema`):

- `catalog_versions(version PK, cost_formula, imported_at, is_active)` — exactly one row has `is_active = 1`.
- `service_points(version, point_id, grid_x, grid_y, barrier_penalty, PK(version, point_id))`
- `point_services(version, point_id, service)` / `point_access(version, point_id, access)`
- `hard_requirements(version, need, access)` — maps a mobility/communication need to a mandatory access capability (e.g. `WHEELCHAIR → STEP_FREE`, `HEARING → SIGN_INTERPRETER`, `SPEECH → TEXT_COMMUNICATION`).
- `closures(version, event_id, point_id, from_ts, to_ts)` — epoch seconds.
- `capability_events(version, event_id, point_id, capability, from_ts, to_ts)` — temporary capability degradations, epoch seconds.
- `home_service(version, allowed_service, reason)` + `home_service_mobility(version, mobility)`
- `home_service_slots(version, slot_id, from_ts, to_ts, capacity, cost)` — bookable home-service appointment windows.
- `bookings(booking_id PK, version, slot_id, snapshot_id, created_at)` — capacity reservations, scoped per catalog version.
- `snapshots(snapshot_id PK, version, kind, request_json, response_json, created_at)` — immutable routing snapshots.

## Endpoints

| Method | Path                | Purpose                                                  |
| ------ | ------------------- | -------------------------------------------------------- |
| GET    | `/health`           | liveness                                                 |
| POST   | `/catalog/import`   | import catalog JSON, atomically activate its version     |
| GET    | `/catalog/active`   | active version summary                                   |
| GET    | `/catalog/versions` | all versions + active flag                               |
| POST   | `/route`            | route one applicant, persist + return snapshot           |
| POST   | `/route/batch`      | `{"requests": [...]}`, one version for the whole batch   |
| GET    | `/snapshots/{id}`   | replay a stored snapshot **verbatim** (never recomputed) |

Route request:

```json
{
  "serviceNeed": "LEGAL_AID",
  "mobility": "WHEELCHAIR", // default "STANDARD"
  "communication": ["HEARING"],
  "origin": { "x": 2, "y": 0 },
  "at": "2026-08-02T12:00:00Z" // optional, defaults to now
}
```

Response contains `snapshotId`, `catalogVersion`, the normalized request,
`candidates` (with `totalCost` and `costBreakdown`), `exclusions` (reason
chain per point), and `homeService` for homebound applicants (with a
`booking` when capacity was reserved).

```bash
curl -X POST localhost:8080/route -H 'content-type: application/json' -d '{
  "serviceNeed":"LEGAL_AID","mobility":"WHEELCHAIR",
  "origin":{"x":2,"y":0},"at":"2026-08-10T00:00:00Z"}'
```

## Routing semantics

Per point, all applicable exclusion reasons are collected (the reason chain),
in this fixed order:

1. `SERVICE_UNAVAILABLE` — point does not offer `serviceNeed`.
2. `MISSING_REQUIRED_ACCESS` (+`capability`) — point lacks a capability required by `hardRequirements` for the applicant's mobility/communication needs.
3. `TEMPORARILY_CLOSED` (+`eventId`, `from`, `to`) — a closure is active at `at`.
4. `CAPABILITY_DEGRADED` (+`capability`, `eventId`, `from`, `to`) — a degradation event is active at `at` and the applicant requires the degraded capability.

Only points with an empty reason chain become candidates. Cost is the catalog
formula implemented natively:

```
totalCost = abs(originGridX - pointGridX) + abs(originGridY - pointGridY) + barrierPenalty
```

**Closures and degradations are half-open `[from, to)`**: at `from` the event
is active (inclusive), at `to` the point is normal again (exclusive). The
boundary instants are covered by `closure_endpoints_are_half_open` and
`degradation_window_and_overlap_priority`.

**Event overlap priority**: when a closure and one or more degradations are
active at the same point at the same time, the closure dominates — the chain
reports `TEMPORARILY_CLOSED` only, since the whole location is unreachable
anyway. The fixture exercises this: `CLOSE-01` closes POINT-C on
`[2026-08-02, 2026-08-04)` while `DEGRADE-01` removes its `SIGN_INTERPRETER`
capability on `[2026-08-03, 2026-08-05)`, so 08-03 is closure-dominated and
`[08-04, 08-05)` reports `CAPABILITY_DEGRADED`. A degradation only excludes
applicants who actually require the degraded capability — a wheelchair user
who does not need `SIGN_INTERPRETER` still reaches POINT-C during DEGRADE-01.
Outside every event window, results are identical to the pre-window baseline.

**Home service (degradation path with booking capacity)**: home service is
never a shortcut around entity routing. Every request first evaluates all
entity points with the normal hard gates, and the exclusion reason chains are
always preserved verbatim in the response. The path triggers only when **all**
of the following hold:

1. `mobility` is in `homeService.allowedMobility` (fixture: `HOMEBOUND`),
2. `serviceNeed == homeService.allowedService` (fixture: `LEGAL_AID`),
3. no entity point survived — and every exclusion chain consists solely of
   hard conditions (`MISSING_REQUIRED_ACCESS`, `TEMPORARILY_CLOSED`,
   `CAPABILITY_DEGRADED`). A point excluded merely for `SERVICE_UNAVAILABLE`
   means the request is a service mismatch, not an accessibility failure, and
   the path stays closed (`HOME_SERVICE_NOT_AVAILABLE`).

If any entity candidate exists, the candidates are returned and
`homeService` is null.

When the path triggers, capacity is reserved atomically: slots (fixture:
`SLOT-AM`/`SLOT-PM`, both cost 2, capacity 1, on 2026-08-06) are scanned in
stable order (cost ascending, then slot id ascending) and the first slot with
`at < to` and remaining capacity is booked inside one IMMEDIATE transaction
together with the snapshot — concurrent requests can never double-book
(`home_service_concurrent_booking_race_never_double_books`). Bookings are
scoped per catalog version, so a hot reload starts a fresh capacity ledger
while old snapshot replays still show their original booking. When no slot
has capacity left, the outcome is `HOME_SERVICE_NO_CAPACITY` with
`booking: null` and an **empty** candidate list — the router never falls back
to entity points that failed hard accessibility gates. Capacity-dependent
outcomes bypass the route cache entirely.

**Tie-breaking** (from the catalog `tieBreak`): `totalCost` ascending, then
point id ascending. Exclusions are ordered by point id. Combined with
normalization of the request (communication list sorted/deduped, timestamps
to second precision) and order-independent SQL reads, permuting the import
point order or request field order cannot change ordering, exclusion
reasons, or cost breakdowns — verified by `input_order_permutation_is_invariant`
and `equal_cost_candidates_tie_break_by_point_id`.

## Hot reload & concurrency

- Import is one write transaction: insert the new version's rows and flip
  `is_active` atomically. Readers on WAL connections see either the complete
  old version or the complete new one — never a mixture.
- Each `/route` and `/route/batch` request pins the version active at its
  start (`active_version_now`); a batch pins once, so every snapshot in one
  batch carries the same `catalogVersion` even if an import commits while the
  batch runs (`batch_pinned_to_one_version_during_hot_reload`: 20 concurrent
  batches racing a version swap).
- Snapshots store the full response JSON at request time. `GET
/snapshots/{id}` returns the stored bytes verbatim, so replaying an old
  snapshot can never mix in data imported later.
- Verified by `hot_reload_keeps_requests_consistent_and_replays_immutable`
  (32 concurrent readers racing a version swap; every response asserts
  version-complete data, old snapshot replays unchanged).

## Query caching

Two in-memory caches sit in front of SQLite:

- **Catalog cache** keyed by version — catalog versions are immutable, so a
  cached catalog is always complete and correct for that version.
- **Route cache** keyed by `(catalog version, normalized request JSON)`. A
  hit returns byte-identical content (modulo a freshly minted `snapshotId`)
  and is still persisted as its own snapshot.

Both caches are **cleared on every successful import**, so a query after a
hot reload can never observe stale pre-import data
(`cache_hit_is_deterministic_and_invalidated_on_reload`).

## Performance record

`large_catalog_performance_record` builds a deterministic 3000-point catalog,
imports it, then runs 100 route requests with distinct origins (route-cache
misses) plus one repeated request (cache hit). Reproduce with:

```bash
cargo test --release large_catalog_performance_record -- --nocapture
```

Observed on this machine (Apple Silicon, release build):

```
PERF_RECORD catalog_points=3000 rounds=100 import_ms=13.2 \
  miss_avg_ms=8.70 miss_p95_ms=19.59 miss_max_ms=97.60 hit_ms=4.33 \
  candidates=200 exclusions=2800
```

(Debug build for the same record: miss avg ≈ 55 ms, p95 ≈ 79 ms.)

Docker is outside the project contract.
