# Legal Service Router

Constraint-based routing to accessible public legal-service locations. Hard
capability gates (service category, mandatory accessibility, temporary
closures) are evaluated **before** any cost comparison — a nearer point that
lacks a required capability is excluded, never preferred. Stack: Rust stable,
Axum, SQLite (bundled, WAL). No external map or route API.

## Source material

`materials/service-catalog.json` fixes service-point IDs, capability names, catalog versions, closure events, and home-service eligibility examples. `POST /catalog/import` accepts exactly this format.

## Build, test, run

```bash
cargo build
cargo test                 # 13 tests: unit + end-to-end API tests
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
- `home_service(version, allowed_service, reason)` + `home_service_mobility(version, mobility)`
- `snapshots(snapshot_id PK, version, kind, request_json, response_json, created_at)` — immutable routing snapshots.

## Endpoints

| Method | Path | Purpose |
|---|---|---|
| GET | `/health` | liveness |
| POST | `/catalog/import` | import catalog JSON, atomically activate its version |
| GET | `/catalog/active` | active version summary |
| GET | `/catalog/versions` | all versions + active flag |
| POST | `/route` | route one applicant, persist + return snapshot |
| POST | `/route/batch` | `{"requests": [...]}`, one version for the whole batch |
| GET | `/snapshots/{id}` | replay a stored snapshot **verbatim** (never recomputed) |

Route request:

```json
{
  "serviceNeed": "LEGAL_AID",
  "mobility": "WHEELCHAIR",        // default "STANDARD"
  "communication": ["HEARING"],
  "origin": {"x": 2, "y": 0},
  "at": "2026-08-02T12:00:00Z"     // optional, defaults to now
}
```

Response contains `snapshotId`, `catalogVersion`, the normalized request,
`candidates` (with `totalCost` and `costBreakdown`), `exclusions` (reason
chain per point), and `homeService` when the applicant is homebound.

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

Only points with an empty reason chain become candidates. Cost is the catalog
formula implemented natively:

```
totalCost = abs(originGridX - pointGridX) + abs(originGridY - pointGridY) + barrierPenalty
```

**Closures are half-open `[from, to)`**: at `from` the point is closed
(inclusive), at `to` it is open again (exclusive). The boundary instants are
covered by `closure_endpoints_are_half_open`.

**Home service**: if `mobility` is in `homeService.allowedMobility`, no
physical point is reachable. If `serviceNeed == allowedService`, the outcome
is `homeService: {eligible: true, reason: "HOME_SERVICE_REQUIRED"}` and every
point is excluded with `HOME_SERVICE_REQUIRED`; otherwise `eligible: false`
with `HOME_SERVICE_NOT_AVAILABLE`.

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
- Each `/route` and `/route/batch` request loads the active catalog inside a
  single read transaction, so one request (and one batch) sees exactly one
  catalog version even while an import commits concurrently.
- Snapshots store the full response JSON at request time. `GET
  /snapshots/{id}` returns the stored bytes verbatim, so replaying an old
  snapshot can never mix in data imported later.
- Verified by `hot_reload_keeps_requests_consistent_and_replays_immutable`
  (32 concurrent readers racing a version swap; every response asserts
  version-complete data, old snapshot replays unchanged).

## Performance record

`large_catalog_performance_record` builds a deterministic 3000-point catalog,
imports it, and runs 100 identical route requests, asserting the result is
stable across rounds. Reproduce with:

```bash
cargo test --release large_catalog_performance_record -- --nocapture
```

Observed on this machine (Apple Silicon, release build):

```
PERF_RECORD catalog_points=3000 rounds=100 import_ms=16.1 \
  route_avg_ms=42.24 route_p95_ms=59.94 route_max_ms=108.58 \
  candidates=200 exclusions=2800
```

(Debug build for the same record: avg ≈ 304 ms, p95 ≈ 414 ms.)

Docker is outside the project contract.

