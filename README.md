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
- **Home service (fallback)**: a *degraded path*, not a shortcut. It is offered
  only when **all** of: the applicant's mobility is `HOMEBOUND`, the requested
  service is `LEGAL_AID`, and **every physical point was excluded** (no reachable
  candidate). The physical exclusion chain is always preserved alongside it.
  See [Home-service fallback & appointment capacity](#home-service-fallback--appointment-capacity).

### Temporary events

Two independent kinds of temporary event affect a point over a half-open
interval `[from, to)`:

- **Closure** (`closures`) — the *whole point* is unavailable. Reason
  `CLOSED:<eventId>`.
- **Capability degradation** (`degradations`) — the point stays open but
  *temporarily loses one access capability* (e.g. a broken lift removes
  `STEP_FREE`). Reason `DEGRADED:<eventId>:<capability>`, emitted only when the
  applicant actually needs that capability.

**Overlap priority — closure dominates degradation.** While a point is closed,
`CLOSED:<eventId>` is the sole temporal reason and any overlapping degradation is
suppressed (an unavailable point's individual capabilities are moot). Only once
the point re-opens do active degradations apply. The fixture ships an
intentionally overlapping pair on `POINT-C`:

| Instant (UTC)          | Active events            | POINT-C outcome (wheelchair → NOTARY) |
| ---------------------- | ------------------------ | ------------------------------------- |
| `< 08-02`              | none                     | candidate                             |
| `[08-02, 08-03)`       | CLOSE-01                 | `CLOSED:CLOSE-01`                     |
| `[08-03, 08-04)`       | CLOSE-01 **+** DEGRADE-01 | `CLOSED:CLOSE-01` (closure wins)     |
| `[08-04, 08-05)`       | DEGRADE-01               | `DEGRADED:DEGRADE-01:STEP_FREE`      |
| `>= 08-05`             | none                     | candidate                             |

If several degradations remove the same needed capability, the lowest `eventId`
is reported (deterministic).

### Home-service fallback & appointment capacity

`HOME_SERVICE_REQUIRED` is a **fallback**, returned only when every physical
point is excluded (never as a shortcut past a reachable point). When eligible,
the router books a home-visit **appointment slot** from a finite pool.

- **Slots** are declared per catalog version: `{slotId, cost, capacity}`. They
  are tried in `(cost ascending, slotId ascending)` order — the same tie-break
  as physical candidates — so two equal-cost slots compete deterministically
  (lower id wins).
- **Capacity** is live per-`(version, slot)` state kept in `home_reservations`,
  *outside* the immutable catalog. Consequences:
  - Reserving is **atomic** (one SQLite transaction), so concurrent requests
    never overbook and capacity can genuinely run out mid-batch.
  - A **catalog version switch starts capacity fresh** (a different version has
    its own reservation rows).
  - Closure/degradation hot-updates rebuild the catalog `Arc` but never disturb
    reservations.
- **Outcomes** (`home_service.status`):
  - `RESERVED` — a slot was booked; `slot_id` and `slot_cost` are included.
  - `NO_CAPACITY` — eligible but every slot is full; `reason` becomes
    `HOME_SERVICE_NO_CAPACITY`. The candidate list stays empty — **a capacity
    shortage never falls back to an inaccessible or closed physical point.**
  - (`ELIGIBLE` is the transient state from the pure router before the store
    resolves capacity; it never appears in an HTTP response.)

Example (all physical points excluded, first eligible request):

```jsonc
{
  "candidates": [],
  "exclusions": [
    {"point_id": "POINT-A", "reasons": ["MISSING_ACCESS:SIGN_INTERPRETER"]},
    {"point_id": "POINT-B", "reasons": ["MISSING_ACCESS:TEXT_COMMUNICATION"]},
    {"point_id": "POINT-C", "reasons": ["CLOSED:CLOSE-01"]}
  ],
  "home_service": {
    "reason": "HOME_SERVICE_REQUIRED",
    "status": "RESERVED",
    "slot_id": "SLOT-AM",
    "slot_cost": 5
  }
}
```

The pure routing function never mutates `hardRequirements` and the round-1
capability mapping is unchanged; the fallback only adds an outcome when the
physical set is empty.

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
| `home_slots`        | Appointment slot definitions `(slotId, cost, capacity)` per version |
| `home_reservations` | Live per-`(version, slot)` reservation counts (capacity state) |
| `closures`          | Temporary closures `[from, to)`, keyed by `(version, event_id)` |
| `degradations`      | Temporary capability losses `[from, to)`, keyed by `(version, event_id)` |
| `active_version`    | Single-row pointer to the currently active version           |
| `snapshots`         | Immutable routing results (request + result JSON)            |

Closures and degradations live in their own tables (not inside the raw blob) so
the start/end endpoints can toggle an event without rewriting the imported
catalog. Every toggle rebuilds that version's normalized catalog and atomically
swaps a fresh `Arc<Catalog>` into the cache (cache invalidation), while any
snapshot taken earlier keeps replaying its own frozen data.

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
`MISSING_ACCESS:<CAPABILITY>` (structural, one per missing capability, sorted),
`CLOSED:<eventId>` (whole point closed), `DEGRADED:<eventId>:<CAPABILITY>`
(capability temporarily lost while the point is open).

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

### `POST /degradations/start` / `POST /degradations/end`

```bash
# begin a temporary capability loss (POINT-A's STEP_FREE goes offline)
curl -X POST localhost:8080/degradations/start -H 'content-type: application/json' -d '{
  "version":"CAT-2026-07-31","event_id":"DEG-A","point_id":"POINT-A",
  "capability":"STEP_FREE","from":"2026-09-01T00:00:00Z","to":"2026-09-10T00:00:00Z"
}'  # 201

# end it early (clamps `to` to `at`; defaults to now)
curl -X POST localhost:8080/degradations/end -H 'content-type: application/json' -d '{
  "version":"CAT-2026-07-31","event_id":"DEG-A","at":"2026-09-03T00:00:00Z"
}'  # 200 {"ended_at":"2026-09-03T00:00:00Z"}
```

Same half-open semantics as closures. During the window the point remains a
routing candidate for applicants who don't need the degraded capability, and is
excluded with `DEGRADED:DEG-A:STEP_FREE` for those who do. See the overlap
priority table above for closure-vs-degradation precedence.

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

`cargo test` runs 23 integration tests. Round 1 covers: hard-filter-before-cost
(closer point excluded), all-hard-capabilities-unsatisfied (empty candidates +
full reason chain), equal-cost tie-break, input-order independence, home-service
fallback gating, half-open closure boundaries, HTTP closure start/end, batch
routing, snapshot isolation under hot reload, concurrency, and a repeatable
performance record.

Round 2 (capability degradation) adds:

- `degradation_timeline_and_overlap_priority` — the full `POINT-C` timeline and
  closure-dominates-degradation precedence in the overlap window.
- `degradation_half_open_boundaries` — behavior exactly at each `from`/`to`.
- `degradation_ignored_when_capability_not_needed` — a degraded capability the
  applicant doesn't need never excludes the point.
- `equal_cost_candidates_with_degradation` — a degradation removes an equal-cost
  candidate rather than reordering ties.
- `http_start_and_end_degradation` — the start/end endpoints round-trip.
- `snapshot_replay_pins_catalog_version_across_degradation_reload` — a snapshot
  replays verbatim while a fresh route on the hot-updated catalog reflects the
  new degradation (cache invalidation proof).
- `batch_pinned_to_single_version_during_degradation_reload` — 300 concurrent
  batches vs. a degradation-toggling reloader; every batch's items agree, so no
  batch mixes pre- and post-degradation data.

Round 3 (home-service fallback + appointment capacity) adds:

- `home_service_is_fallback_only` — home service triggers only when every
  physical point is excluded; a reachable point yields no home outcome, and the
  physical exclusion chain is preserved.
- `home_capacity_exhausts_mid_batch` — a 3-request batch against 2 slots reserves
  two then returns `NO_CAPACITY`, never listing the inaccessible point.
- `two_equal_cost_slots_compete_deterministically` — equal-cost slots are booked
  lowest-id-first, independent of declaration order.
- `version_switch_resets_capacity` — exhausted capacity on one version; a fresh
  version reserves again.
- `no_capacity_never_falls_back_to_inaccessible_point` — zero capacity yields
  `NO_CAPACITY` with empty candidates; the hard-excluded point is never offered.
- `concurrent_reservations_never_overbook` — 20 concurrent requests vs. capacity
  3: exactly 3 reserve, 17 get `NO_CAPACITY` (atomic reservation).

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
