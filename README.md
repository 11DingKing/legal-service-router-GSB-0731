# Legal Service Router

Constraint-based routing to accessible public legal-service locations. Routing
never optimizes distance alone: a candidate must first clear **hard**
service-category and accessibility checks (and not be temporarily closed) before
its deterministic reachability cost is ever compared. A geographically closer
point that lacks a required capability is *excluded with a reason*, not
preferred.

Built with Rust stable, Axum, and SQLite (bundled). No external map or route API
is used — costs are computed from the catalog's own grid + barrier formula.

## Source material

`materials/service-catalog.json` fixes service-point IDs, capability names,
catalog versions, closure events, and home-service eligibility examples. It is
the authoritative import shape and is used verbatim as the boot seed and the
test fixture.

## Quick start

```bash
cargo test          # unit + integration tests (in-memory SQLite)
cargo run           # starts the HTTP server, seeding materials/service-catalog.json
```

Environment variables (all optional):

| Var            | Default                          | Meaning                                   |
| -------------- | -------------------------------- | ----------------------------------------- |
| `DB_PATH`      | `router.db`                      | SQLite file path (`:memory:` for tests)   |
| `BIND`         | `127.0.0.1:8080`                 | Listen address                            |
| `SEED_CATALOG` | `materials/service-catalog.json` | Catalog imported+activated on first boot  |

## Domain model

A **catalog version** (e.g. `CAT-2026-07-31`) bundles service points, the
`hardRequirements` map (applicant need → mandatory access capability), the cost
formula, tie-break rule, closures, and home-service policy. Each version is
immutable once imported.

- **Hard requirements** (from the fixture): `WHEELCHAIR → STEP_FREE`,
  `HEARING → SIGN_INTERPRETER`, `SPEECH → TEXT_COMMUNICATION`.
- **Cost formula**:
  `abs(originX-pointX) + abs(originY-pointY) + barrierPenalty` (Manhattan grid
  distance plus the point's barrier penalty). No external routing.
- **Home service**: a `HOMEBOUND` applicant requesting the allowed service
  (`LEGAL_AID`) is flagged `HOME_SERVICE_REQUIRED` in the result.

## Schema setup

SQLite is the source of truth; the connection is opened (and the schema created)
automatically on startup — no migration step is required. Tables:

| Table               | Purpose                                                        |
| ------------------- | ------------------------------------------------------------- |
| `catalog_versions`  | One row per imported version (raw JSON + cost formula + tie-break) |
| `points`            | Point id, grid, barrier penalty, keyed by `(version, id)`     |
| `point_services`    | Service categories offered per point                          |
| `point_access`      | Accessibility capabilities per point                          |
| `hard_requirements` | Need-key → required capability, per version                   |
| `home_service` / `home_mobility` | Home-service policy per version                  |
| `closures`          | Temporary closures `[from, to)`, keyed by `(version, event_id)` |
| `active_version`    | Single-row pointer to the currently active version           |
| `snapshots`         | Immutable routing results (request + result JSON)            |

Closures live in their own table (not inside the raw blob) so the start/end
endpoints can toggle a closure without rewriting the imported catalog.

## Endpoints

### `POST /catalog/import` — import a version (hot reload)

```bash
curl -X POST localhost:8080/catalog/import -H 'content-type: application/json' \
  -d '{"catalog": { ...service-catalog.json shape... }, "activate": true}'
# 201 {"catalog_version":"CAT-2026-07-31","activated":true}
```

Re-importing an existing version id is rejected (`409`) so snapshots stay
reproducible.

### `POST /catalog/activate` — switch active version

```bash
curl -X POST localhost:8080/catalog/activate -H 'content-type: application/json' \
  -d '{"version":"CAT-2026-08-15"}'      # 204
```

### `GET /catalog/versions`

```bash
curl localhost:8080/catalog/versions
# {"versions":["CAT-2026-07-31"],"active":"CAT-2026-07-31"}
```

### `POST /route` — route + persist an immutable snapshot

```bash
curl -X POST localhost:8080/route -H 'content-type: application/json' -d '{
  "origin": [2, 1],
  "service": "LEGAL_AID",
  "mobility": "WHEELCHAIR",
  "communication": [],
  "at": "2026-07-01T00:00:00Z"
}'
```

```jsonc
{
  "snapshot_id": "…uuid…",
  "result": {
    "catalog_version": "CAT-2026-07-31",
    "origin": [2, 1],
    "service": "LEGAL_AID",
    "mobility": "WHEELCHAIR",
    "communication": [],
    "evaluated_at": "2026-07-01T00:00:00Z",
    "candidates": [
      {"point_id": "POINT-A", "grid": [1,1], "cost": {"manhattan":1,"barrier_penalty":0,"total":1}},
      {"point_id": "POINT-C", "grid": [5,5], "cost": {"manhattan":7,"barrier_penalty":0,"total":7}}
    ],
    "exclusions": [
      {"point_id": "POINT-B", "reasons": ["MISSING_ACCESS:STEP_FREE"]}
    ]
  }
}
```

Even though the applicant sits exactly on POINT-B (distance 0), POINT-B is
excluded for missing `STEP_FREE`; POINT-A wins. `at` is optional and defaults to
request time; supply it to evaluate closures deterministically.

**Exclusion reason codes** (order-stable): `MISSING_SERVICE`,
`MISSING_ACCESS:<CAPABILITY>` (one per missing capability, sorted),
`CLOSED:<eventId>`.

### `POST /route/batch` — batch over one captured version

```bash
curl -X POST localhost:8080/route/batch -H 'content-type: application/json' -d '{
  "requests": [
    {"origin":[0,0],"service":"LEGAL_AID","at":"2026-07-01T00:00:00Z"},
    {"origin":[5,5],"service":"MEDIATION","at":"2026-07-01T00:00:00Z"}
  ],
  "persist": true
}'
```

The whole batch captures **one** catalog version up front, so every item is
evaluated against the same complete snapshot even if a hot reload lands
mid-batch.

### `POST /closures/start` / `POST /closures/end`

```bash
# begin a temporary closure
curl -X POST localhost:8080/closures/start -H 'content-type: application/json' -d '{
  "version":"CAT-2026-07-31","event_id":"CLOSE-A","point_id":"POINT-A",
  "from":"2026-09-01T00:00:00Z","to":"2026-09-10T00:00:00Z"
}'  # 201

# end it early (clamps `to` to `at`; defaults to now)
curl -X POST localhost:8080/closures/end -H 'content-type: application/json' -d '{
  "version":"CAT-2026-07-31","event_id":"CLOSE-A","at":"2026-09-03T00:00:00Z"
}'  # 200 {"ended_at":"2026-09-03T00:00:00Z"}
```

Closures use **half-open** intervals `[from, to)`: the point is closed *at*
`from` and open again exactly *at* `to`. Ending clamps `to`, so a point re-opens
at the effective end instant. Both operations rebuild that version's in-memory
catalog and atomically swap it in.

### `GET /snapshots/:id` — replay verbatim

```bash
curl localhost:8080/snapshots/<uuid>
```

Returns the stored result document exactly as computed, including its
`catalog_version`. Replay never re-runs routing, so a snapshot taken against an
old version can never mix in data from a version imported later.

## Tie-breaking

Candidates are sorted by the catalog's `tieBreak` rule
`["totalCost ascending", "point id ascending"]`. Because point ids are unique
this is a **total order**, so results are fully stable: equal-cost candidates are
always ordered by ascending id, independent of the order points were imported or
needs were supplied. Exclusions are emitted in point-id order and each point's
missing-capability reasons in sorted capability order.

## Hot reload

`POST /catalog/import` (and closure start/end) build a fresh immutable
`Catalog`, wrap it in an `Arc`, and swap it into an in-memory cache guarded by an
`RwLock`. Imports are transactional in SQLite and only published to the cache
after commit. Activating or importing a new version changes which `Arc` future
requests capture; in-flight requests are unaffected.

## Concurrency model

A `Catalog` is immutable. A routing request clones the active version's `Arc`
under a brief read lock and then releases the lock, holding a complete,
self-consistent version for its entire lifetime:

- A single request (or batch) always sees **exactly one** complete catalog
  version — never a half-applied mix — even while a concurrent hot reload swaps
  in a newer one.
- Old snapshots replay from stored JSON and cannot absorb newer data.
- The SQLite connection is serialized behind a mutex; the read-mostly catalog
  cache uses an `RwLock` so many routes proceed in parallel while reloads take
  the write lock only for the pointer/`Arc` swap.

The `concurrent_hot_reload_and_queries_are_consistent` test drives 50 hot
reloads against 200 concurrent routes and asserts every candidate belongs to the
single captured version and satisfies the hard capability.

## Tests

`cargo test` runs 11 integration tests covering: hard-filter-before-cost (closer
point excluded), all-hard-capabilities-unsatisfied (empty candidates + full
reason chain), equal-cost tie-break, input-order independence, home-service
eligibility, half-open closure boundaries, HTTP closure start/end, batch
routing, snapshot isolation under hot reload, concurrency, and a repeatable
performance record.

### Performance record

`perf_larger_catalog_is_repeatable` builds a 5,000-point catalog and measures
routing latency while asserting identical output across runs. Representative
local result:

```
[perf] catalog_points=5000 candidates=5000 iterations=200 total≈899ms per_route≈4.5ms
```

Re-run with `cargo test --test integration perf_larger_catalog_is_repeatable -- --nocapture`.

## Delivery contract

- Rust stable, Axum, SQLite; no external map or route API. ✔
- Filter mandatory service + accessibility capabilities before comparing
  deterministic reachability cost. ✔
- Persist immutable routing snapshots tied to a catalog version + exclusion
  reason chain. ✔
- Native verification: `cargo test` and `cargo run`. ✔
- Documented schema setup, endpoint examples, tie-breaking, hot reload, and
  concurrency. ✔

Docker is outside the project contract.
