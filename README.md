# Legal Service Router

Constraint-based routing to accessible public legal-service locations. Public legal
service routing must not be decided by proximity alone: a wheelchair user needs a
step-free entrance, a Deaf applicant may need a sign-language interpreter, and a
service point may be temporarily closed. This service imports versioned service
catalogs and produces **immutable routing snapshots** bound to a single catalog
version.

- **Stack:** Rust stable, [Axum](https://github.com/tokio-rs/axum), SQLite
  (`rusqlite` + `r2d2`). No external map or routing API is called.
- **Determinism:** hard capability filtering always runs before cost comparison;
  results are sorted by a stable tie-break and persisted as an immutable snapshot.
- **Concurrency:** readers and a single writer run concurrently under SQLite WAL;
  every request pins exactly one catalog version, and old snapshots never see new
  data.

## Source material

[materials/service-catalog.json](materials/service-catalog.json) fixes
service-point IDs, capability names, the catalog version, a closure event, the
cost formula, tie-break rules, and home-service eligibility. It is imported
automatically on first run (if no catalog is active) and may be re-POSTed via the
admin API.

## Quick start

```bash
cargo run                 # starts on 0.0.0.0:8080, auto-imports the seed catalog
cargo test                # runs 30 unit + 23 integration tests
```

Configuration (environment variables):

| Variable       | Default            | Description                              |
|----------------|--------------------|------------------------------------------|
| `DATABASE_URL` | `legal-router.db`  | SQLite file path                         |
| `BIND_ADDR`    | `0.0.0.0:8080`     | Listen address                           |
| `RUST_LOG`     | `info,...`         | Tracing filter                           |

## Routing model

### 1. Hard capability filtering (before any cost comparison)

For each request the applicant's needs are resolved through the catalog's
`hardRequirements` map:

| Need code     | Required access       |
|---------------|-----------------------|
| `WHEELCHAIR`  | `STEP_FREE`           |
| `HEARING`     | `SIGN_INTERPRETER`    |
| `SPEECH`      | `TEXT_COMMUNICATION`  |

Needs may be supplied under `mobility` (e.g. `WHEELCHAIR`, `HOMEBOUND`) or
`communication` (e.g. `HEARING`, `SPEECH`). Duplicate needs collapse to a single
required access code. A point that is closer but lacks a mandatory capability is
**excluded**, never preferred.

A point is a candidate only when **all** of the following hold:

1. It offers the requested `service`.
2. It satisfies **every** required access capability.
3. It is **not** temporarily closed at `queryTime`.

### 2. Reachability cost

For every surviving candidate the cost is computed from the catalog formula:

```
totalCost = |originGridX - pointGridX| + |originGridY - pointGridY| + barrierPenalty
```

The response breaks the cost into `distance` (Manhattan) and `barrierPenalty`.

### 3. Stable tie-break

Candidates are sorted by:

1. `totalCost` ascending
2. `pointId` ascending (lexicographic)

Excluded points are returned sorted by `pointId` ascending, and their exclusion
reasons are emitted in a fixed order (`MISSING_SERVICE`, `MISSING_ACCESS`,
`CAPABILITY_DEGRADED`, `TEMPORARILY_CLOSED`) so output is identical regardless
of import/input order.

### Temporary closures

A closure is a half-open interval **`[from, to)`**:

- at exactly `from` the point **is closed**;
- at exactly `to` the point **is open** again.

This makes the start/end boundary cases explicit and testable.

### Capability degradations

A degradation event temporarily removes one or more accessibility capabilities
from a point during a half-open interval `[from, to)`, without closing the point
entirely. For example, a sign-language interpreter may be unavailable for two
days while the building remains open for other services.

```json
{
  "eventId": "DEGRADE-01",
  "pointId": "POINT-C",
  "from": "2026-08-03T00:00:00Z",
  "to": "2026-08-05T00:00:00Z",
  "removedAccess": ["SIGN_INTERPRETER"]
}
```

During a degradation window:

- If an applicant requires a removed capability, the point is excluded with
  reason `CAPABILITY_DEGRADED` (including the event id, the affected access
  code, and the window).
- If the applicant does not require the removed capability, the point remains a
  candidate; its effective `access` list in the response reflects the degraded
  state.
- A capability that the point never offered statically still reports
  `MISSING_ACCESS` rather than `CAPABILITY_DEGRADED`.

**Overlap priority.** When a closure and a degradation overlap on the same
point, the **closure takes precedence**: the point is reported as
`TEMPORARILY_CLOSED` and no `CAPABILITY_DEGRADED` reason is emitted (the point
is fully unavailable, so the partial degradation is irrelevant). In the seed
catalog `CLOSE-01` runs Aug 2–4 and `DEGRADE-01` runs Aug 3–5; they overlap on
Aug 3, where closure wins.

### Home service downgrade path

Home service is a **fallback**, not a parallel option. It is offered only when
**all three** conditions hold:

1. The requested service equals `homeService.allowedService` (`LEGAL_AID`).
2. The applicant's `mobility` includes an allowed mobility code (`HOMEBOUND`).
3. **Every physical service point is excluded** (by missing service, missing
   hard capability, capability degradation, or temporary closure).

When these hold, the response contains `candidates: []`, the full `excluded`
array with every physical point's exclusion reasons **preserved**, and a
`homeService` block:

```json
{
  "eligible": true,
  "reason": "HOME_SERVICE_REQUIRED",
  "slot": {
    "slotId": "SLOT-MORNING",
    "totalCost": 0,
    "costBreakdown": { "distance": 0, "barrierPenalty": 0 },
    "grid": [2, 2]
  }
}
```

If any physical candidate survives the hard filters, `homeService` is omitted
entirely (the applicant can visit a point, so no home visit is needed).

**No fallback to inaccessible points.** When home service capacity is
exhausted, the response returns `candidates: []` and a `noCapacity` block — it
never relaxes hard accessibility requirements to surface a non-compliant
physical point:

```json
{
  "eligible": true,
  "reason": "HOME_SERVICE_REQUIRED",
  "noCapacity": {
    "code": "HOME_SERVICE_NO_CAPACITY",
    "message": "all home service appointment slots are at capacity"
  }
}
```

### Appointment slots and capacity

Home service slots are defined per catalog version under
`homeService.slots`:

```json
{
  "slotId": "SLOT-MORNING",
  "grid": [2, 2],
  "capacity": 2,
  "barrierPenalty": 0
}
```

- Slots are sorted by the same tie-break as physical candidates:
  `(totalCost ascending, slotId ascending)`, where cost is Manhattan distance
  from `originGrid` to the slot grid plus `barrierPenalty`.
- Capacity is decremented **atomically** inside the snapshot transaction using
  `UPDATE ... WHERE remaining_capacity > 0`. Concurrent requests compete
  safely; exactly `capacity` requests reserve a slot, the rest receive
  `HOME_SERVICE_NO_CAPACITY`.
- If the best-cost slot is full, the next slot is tried in order.
- Capacity is **isolated per catalog version**. Importing a new version resets
  capacity independently; old V1 reservations do not affect V2 slots.
- The reserved slot (or no-capacity status) is stored in the immutable snapshot
  and returned on replay.

## HTTP API

### `POST /admin/import`

Import a new catalog version. The whole import is one transaction; the new
version becomes active atomically. A duplicate `catalogVersion` returns `409`.

```bash
curl -s -X POST localhost:8080/admin/import \
  -H 'content-type: application/json' \
  --data @materials/service-catalog.json
```

### `GET /admin/active`

Returns the active catalog version.

### `POST /route`

Compute a routing snapshot for one applicant.

```bash
curl -s -X POST localhost:8080/route \
  -H 'content-type: application/json' \
  -d '{
    "originGrid": [2, 2],
    "service": "LEGAL_AID",
    "mobility": ["WHEELCHAIR"],
    "communication": ["HEARING"],
    "queryTime": "2026-08-05T00:00:00Z"
  }'
```

`queryTime` is optional and defaults to the server's current time. The response
contains `snapshotId`, `catalogVersion`, `candidates` (ranked, with cost
breakdown), `excluded` (with reasons), `tieBreak`, and an optional
`homeService`.

Exclusion reason objects:

```json
{ "code": "MISSING_SERVICE", "service": "LEGAL_AID" }
{ "code": "MISSING_ACCESS", "required": "STEP_FREE" }
{ "code": "CAPABILITY_DEGRADED", "eventId": "DEGRADE-01", "access": "SIGN_INTERPRETER", "from": "...", "to": "..." }
{ "code": "TEMPORARILY_CLOSED", "eventId": "CLOSE-01", "from": "...", "to": "..." }
```

### `POST /route/batch`

Batch multiple requests. All requests in one batch are evaluated against the
**same** catalog version (the active version captured when the batch starts), and
each request gets its own snapshot.

```json
{ "requests": [ { "originGrid": [2,2], "service": "LEGAL_AID" }, ... ] }
```

### `GET /snapshots/:id`

Replay a previously stored routing snapshot. The response is reconstructed from
the persisted snapshot rows only, so even after the active catalog changes, an
old snapshot returns its original candidates, exclusions, costs, and catalog
version.

## Database schema

SQLite is opened in WAL mode with `synchronous=NORMAL`, `busy_timeout=15000`, and
foreign keys on. All tables are keyed by `catalog_version`, which is what makes
version pinning and immutable replay possible.

- `catalog_versions` — version, cost formula, tie-break, import timestamp.
- `service_points` — point id, grid (`grid_x`, `grid_y`), barrier penalty per
  catalog version.
- `point_services` — services offered by a point.
- `point_access` — accessibility capabilities of a point.
- `closures` — temporary closure events (`event_id`, `point_id`, `from`, `to`).
- `degradations` — capability degradation events (`event_id`, `point_id`,
  `from`, `to`, `removed_access` as a JSON array); partially overlapping with
  closures is allowed, with closure taking priority.
- `hard_requirements` — need code -> required access code mapping.
- `home_service_config` — allowed service, allowed mobility, reason.
- `home_service_slots` — appointment slots per catalog version (`slot_id`,
  grid, `total_capacity`, `remaining_capacity`, `barrier_penalty`); capacity is
  atomically decremented on each home-service reservation.
- `active_catalog` — single-row table (`id = 1`) holding the active version.
- `routing_snapshots` — snapshot id, catalog version, original request JSON,
  query time, home-service result, creation time.
- `snapshot_candidates` — ranked candidates per snapshot with the full cost
  breakdown and the point's services/access at snapshot time.
- `snapshot_excluded` — excluded points and their reason JSON per snapshot.

Indexes cover `(catalog_version)` on points, services, access, and snapshots.

## Versioning, hot reload, and concurrency

- **Atomic import:** a new catalog (points, services, access, closures, hard
  requirements, home-service config, and the active pointer) is inserted in a
  single `IMMEDIATE` transaction. Readers never see a partially imported version.
- **Per-request version pinning:** a route reads the active version once, then
  every data access filters by that version. A concurrent import that commits
  mid-request cannot mix data into the response.
- **Append-only cache:** loaded catalogs are cached in memory behind
  `version -> Arc<Catalog>`. Catalog versions are immutable, so the cache is
  append-only and needs no invalidation.
- **Snapshot immutability:** snapshots store their own candidate/excluded rows.
  Replay reads those rows; it never recomputes against the current catalog, so an
  old snapshot cannot leak new points, changed closures, or altered capabilities.
- **WAL + busy timeout:** concurrent reads proceed while a write transaction
  commits; writers serialize via SQLite write lock with a 15s busy timeout.

## Determinism guarantees

- Candidate ordering: `(totalCost, pointId)` — independent of input order.
- Excluded ordering: by `pointId`; reasons within a point follow a fixed code
  order.
- Services/access in responses are emitted in sorted order.
- The same request against the same catalog version always produces byte-identical
  candidate/excluded/cost content (verified by the input-order-independence and
  repeatable-performance tests).

## Project layout

- [src/lib.rs](src/lib.rs) — crate root.
- [src/db.rs](src/db.rs) — schema, connection pool, transactional import,
  versioned catalog loading, snapshot persistence and replay.
- [src/catalog.rs](src/catalog.rs) — catalog types, JSON parsing, validation,
  RFC3339 time handling.
- [src/routing.rs](src/routing.rs) — pure routing engine: hard filtering, cost
  calculation, tie-break, exclusion reasons, home service.
- [src/api.rs](src/api.rs) — Axum router, handlers, append-only catalog cache.
- [src/main.rs](src/main.rs) — server bootstrap and seed import.
- [tests/integration_test.rs](tests/integration_test.rs) — HTTP-level tests for
  import, routing, closures, batch, snapshot replay, hot reload, concurrency,
  and repeatable large-catalog performance.

## Testing

```bash
cargo test                 # all tests
cargo test -- --nocapture  # includes large-catalog timing output
```

The suite covers, among other things:

- hard-capability filtering overriding proximity;
- cost ties broken by point id;
- closure start/end boundary semantics;
- capability-degradation start/end boundaries and `CAPABILITY_DEGRADED` reasons;
- closure/degradation overlap priority (closure wins);
- zero candidates when no point satisfies all hard requirements;
- home service downgrade only when all physical points are excluded;
- home service slot tie-break by `(cost, slotId)`;
- home service capacity exhaustion returning `HOME_SERVICE_NO_CAPACITY`;
- concurrent capacity exhaustion (10 requests, 3 slots, exactly 3 reserved);
- no fallback to inaccessible physical points when capacity is exhausted;
- capacity isolated per catalog version;
- snapshot replay preserving reserved slot and no-capacity state;
- batch routing pinned to one version, including during a concurrent hot update;
- input-order independence;
- duplicate version conflict (`409`);
- snapshot replay preserving degradation and closure state across versions;
- query-cache correctness after hot reload (new version served, old not leaked);
- hot reload concurrent with 60 in-flight queries (every response is internally
  consistent and old snapshots stay on their original version);
- a 400-point catalog run twice with identical, repeatable results.
