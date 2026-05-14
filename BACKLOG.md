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
- **"New" badge** on packs younger than ~30 days. Pure derived
  field from `createdAt`. No schema change.
- **"Updated recently" badge** when a pack published a new version
  in the last N days. Tracks `latest_version` change date.
- **Editor's picks / staff featured** — one boolean (`is_featured`)
  on the packs table, manually flipped by an admin. Shows up at
  the top of the catalog. Lets us curate before there's organic
  signal.
- **Install-count pill** on browse cards — display `downloadCount`
  inline next to file count. Data is already there; just unhidden.

## Profiles / library

- **Per-file content cache GC** — the blob cache at
  `<app-data>/store/blobs/` grows monotonically today. Add a
  "Clean cache" action in Settings + a periodic background sweep
  that removes blobs unreferenced by any profile sidecar.
- **`-userdatafolder` profile switching** — would make switching
  instant (Mods/Saves/Worlds via launch arg instead of folder
  copy). Need to confirm V2.6 client honors it.
- **Clone profile** — "Duplicate this profile" action so a user
  can fork Default into KitsuneDen with the same starting state,
  then drift them independently.

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
