# Backlog

Ideas worth doing — captured here so they survive between sessions
without proliferating GitHub issues for things that aren't yet a
commitment. Roughly ordered by impact-per-line-of-code as I see it.

## Discovery signals

The catalog doesn't have reviews and probably won't for a while (see
the README "Status" note). These derived signals reuse data we
already have and make the catalog feel alive without any user-input
features.

- **"Popular this week"** — sort by 7-day install delta. Needs an
  `install_events` table on the cloud (or a `weekly_installs` column
  rolled by a weekly cron), then a `?sort=trending` query param on
  `/api/v1/packs`.
- **Editor's picks / staff featured** — one boolean (`is_featured`)
  on the packs table, manually flipped by an admin. Shows up at
  the top of the catalog. Lets us curate before there's organic
  signal.
- ~~**Sort selector on Browse Servers**~~ — shipped: client-side
  Players / Newest / Favorites chips mirroring the pack browse,
  persisted into the filter state.

## Profiles / library

- ~~**Per-file content cache GC**~~ — shipped: Settings → Cache
  section reads `cache_stats` (totals + reclaimable bytes) and
  "Clean cache" runs `cache_gc` which walks profile sidecars to
  collect referenced sha256s and removes every cached blob not
  in that set. Empty prefix dirs get swept too. Background sweep
  on launcher startup not yet wired — manual button only today.
- **`-userdatafolder` profile switching** — would make switching
  instant (Mods/Saves/Worlds via launch arg instead of folder
  copy). Need to confirm V2.6 client honors it.
- ~~**Clone profile**~~ — shipped: Duplicate (⎘) glyph on each
  ProfileCard forks mods + saves + worlds into a new profile
  named `<src> (copy)` (auto-bumped on collision). Pack binding
  is preserved; snapshot history starts fresh.

## Launcher chrome / mockup parity

These are tracked against the all-up Dashboard mockup we worked
toward earlier in the session.

- **Active-downloads dock** at the bottom of the LeftRail showing
  in-flight install progress. Needs InstallView state hoisted to
  App.
- **Notifications surface** (the bell in the mockup's top-right).
  Requires a notifications/events store on the cloud — per-user
  push from publisher new-version events, server status changes,
  etc.
- **Activity feed** on the Dashboard. Same backend dependency.
- **Live server stats** (CPU/RAM/Players sparklines on My Servers
  cards). Requires a metrics endpoint + heartbeat enrichment.

## Build / distribution

- **Code signing** — OV or EV Authenticode cert for Windows
  builds. EV gives instant SmartScreen reputation; OV builds rep
  over time. Workflow's already structured to take the cert env
  vars when they're ready.
- **macOS notarization + signing** — Apple Developer ID. Same
  shape; current builds open with right-click → Open.
- **Auto-update** via Tauri's updater plugin. Pulls the latest
  signed manifest from GitHub Releases, applies via the bundled
  updater. Needs signing first.

## Cloud

- **Search**: name-only `ilike` is fine at our scale. When the
  catalog grows past a few hundred packs, swap to Postgres FTS
  or a real search service.
- **Pagination**: catalog endpoints cap at `?limit=100` today.
  Need cursor-based paging once we cross that.
- **Webhooks**: publishers want notification when a downstream
  consumer (server) pins their pack. Webhook-out from the cloud.
- **Rate limiting**: catalog endpoints are unauthenticated +
  uncached past their stale-while-revalidate window. Add a
  per-IP token bucket if abuse becomes a thing.
