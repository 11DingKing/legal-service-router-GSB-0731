# Legal Service Router

Constraint-based routing to accessible public legal-service locations. Built with Rust stable, Axum, and SQLite. No external map or route APIs are used.

## Run

```bash
cargo test
cargo run
```

The server listens on `0.0.0.0:3000` by default. Override with environment variables:

- `BIND_ADDR` — address to bind (default `0.0.0.0:3000`)
- `DATABASE_URL` — SQLite file path (default `legal-router.db`)

Import the bundled catalog:

```bash
curl -X POST http://127.0.0.1:3000/catalog \
  -H "Content-Type: application/json" \
  -d @materials/service-catalog.json
```

## Domain model

The router loads a versioned catalog JSON that contains:

- `catalogVersion` — immutable version string pinned to every snapshot.
- `points[]` — service point ID, Manhattan grid coordinate, offered services, accessibility capabilities, `barrierPenalty`.
- `hardRequirements` — maps applicant mobility/communication needs to required accessibility capabilities (`WHEELCHAIR → STEP_FREE`, `HEARING → SIGN_INTERPRETER`, `SPEECH → TEXT_COMMUNICATION`).
- `closures[]` — temporary point closures with inclusive `from` and exclusive `to` timestamps.
- `homeService` — when an applicant is `HOMEBOUND` and requests the eligible service, all in-person points are excluded with reason `HOME_SERVICE_REQUIRED`.
- `costFormula` — `|originX-pointX| + |originY-pointY| + barrierPenalty` (Manhattan distance plus barrier penalty).
- `tieBreak` — `totalCost ascending`, then `point id ascending`.

## SQLite schema

The schema is created automatically on startup (see [db.rs](file:///Users/huangding/Documents/GSB%203/0731/legal-service-router-GSB-0731-Tony/src/db.rs)).

```sql
CREATE TABLE catalogs (
    catalog_version TEXT PRIMARY KEY,
    payload         TEXT NOT NULL,
    imported_at     TEXT NOT NULL
);

CREATE TABLE snapshots (
    snapshot_id      TEXT PRIMARY KEY,
    catalog_version  TEXT NOT NULL,
    request_payload  TEXT NOT NULL,
    result_payload   TEXT NOT NULL,
    created_at       TEXT NOT NULL
);

CREATE TABLE current_catalog (
    id              INTEGER PRIMARY KEY CHECK (id = 1),
    catalog_version TEXT NOT NULL
);
```

- `catalogs` retains every imported version so old snapshots can be replayed without seeing new data.
- `current_catalog` is a single-row pointer used by routing requests.
- `snapshots` stores the full serialized `RouteResponse`, including `catalogVersion`, candidates, cost breakdown, and the exclusion/reason chain.

## HTTP API

### `POST /catalog`

Import or replace a catalog version. If the same `catalogVersion` is posted again, its payload and import time are updated. A new version string atomically swaps the `current_catalog` pointer.

Request body: the catalog JSON (same shape as `materials/service-catalog.json`).

### `GET /catalog`

Returns a summary of the currently active catalog: version, point count, closure count, import timestamp.

### `POST /route`

Generate an immutable routing snapshot against the current catalog.

Request:

```json
{
  "originGrid": [1, 1],
  "service": "LEGAL_AID",
  "mobility": ["WHEELCHAIR"],
  "communication": ["HEARING"],
  "requestTime": "2026-08-03T12:00:00Z"
}
```

- `mobility` may contain `WHEELCHAIR`, `HOMEBOUND`, `AMBULATORY`.
- `communication` may contain `HEARING`, `SPEECH`, `NONE`.
- `requestTime` is optional; defaults to the server's current time. It is the reference time for evaluating closure windows.

Response:

```json
{
  "snapshotId": "7f341207-a381-4290-ab20-2885cc20191b",
  "catalogVersion": "CAT-2026-07-31",
  "candidates": [
    {
      "pointId": "POINT-A",
      "totalCost": 0,
      "distance": 0,
      "barrierPenalty": 0
    }
  ],
  "exclusions": [
    {
      "pointId": "POINT-B",
      "reason": "MISSING_ACCESSIBILITY",
      "detail": "point lacks required accessibility capability: STEP_FREE"
    },
    {
      "pointId": "POINT-C",
      "reason": "TEMPORARILY_CLOSED",
      "detail": "closure event CLOSE-01 active from 2026-08-02 00:00:00 UTC to 2026-08-04 00:00:00 UTC"
    }
  ]
}
```

### `POST /route/batch`

Submit `{ "requests": [ ... ] }`. Each request produces an independent `RouteResponse` with its own `snapshotId`. Each sub-request reads the same currently active catalog version for that batch call.

### `GET /snapshots/:id`

Replay a previously persisted snapshot. The response is the exact `RouteResponse` captured at routing time and always carries its original `catalogVersion`. Hot-reloading a newer catalog never mutates an existing snapshot.

## Filtering and cost order

The routing pipeline in [router.rs](file:///Users/huangding/Documents/GSB%203/0731/legal-service-router-GSB-0731-Tony/src/router.rs) applies hard gates **before** any cost comparison:

1. Service category must be offered.
2. Every required accessibility capability (`STEP_FREE`, `SIGN_INTERPRETER`, `TEXT_COMMUNICATION`) derived from mobility and communication must be present.
3. If the applicant is eligible for home service, all in-person points are excluded.
4. Active temporary closures at `requestTime` exclude the point.
5. Only then is `totalCost = Manhattan distance + barrierPenalty` computed.

A closer point that fails a hard gate is never returned as a candidate and appears in `exclusions` with a specific reason. Cost is never used to override a missing capability.

## Tie-breaking and determinism

Candidates are sorted by `(totalCost ASC, pointId ASC)`. Exclusions are sorted by `pointId ASC`. The algorithm iterates over the catalog's point list but re-sorts outputs, so changing the input order of points cannot change the ranking, cost breakdown, or exclusion reasons. A 5,000-point determinism test is included in [tests.rs](file:///Users/huangding/Documents/GSB%203/0731/legal-service-router-GSB-0731-Tony/src/tests.rs).

## Closure semantics

For each closure, a point is excluded when `requestTime >= from && requestTime < to`. This means:

- `from` is inclusive — a request exactly at the start timestamp sees the closure.
- `to` is exclusive — a request exactly at the end timestamp does not see the closure.

## Hot reload and concurrency

- Catalog imports are transactional: the catalog payload and `current_catalog` pointer are updated in one SQLite transaction.
- Routing requests load the active catalog once and bind it to the returned snapshot. A concurrent import that lands after the load cannot change the response.
- Each snapshot row stores the full serialized response and its `catalogVersion`. Replaying `GET /snapshots/:id` returns the original result even after newer catalogs have been imported.
- SQLite is opened with WAL and a busy timeout, and all access is serialized through a single `Mutex<Connection>` to keep catalog/version reads consistent under concurrent requests and imports.

## Tests

`cargo test` covers:

- Missing service and missing accessibility gates.
- Wheelchair and hearing hard requirements overriding distance.
- All-hard-capabilities-fail scenario.
- Closure window start/end boundary (inclusive start, exclusive end).
- Home-service eligibility.
- Cost breakdown (`distance`, `barrierPenalty`, `totalCost`).
- Stable tie-breaking by point ID.
- Determinism when input point order is reversed.
- Snapshot immutability across a hot reload to a new catalog version.
- Concurrent routing queries all observing one catalog version.
- Batch routing with independent responses.
- A 5,000-point catalog for repeatable ranking shape.
