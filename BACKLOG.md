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
- ~~**Editor's picks / staff featured**~~ — shipped: admin-only
  toggle on `/account/packs/[id]/edit` (gated by `canAdminister`)
  flips a new `packs.is_featured` bool. The catalog API + cloud
  `/browse` + launcher `BrowseView` all order featured first via
  `desc(packs.isFeatured)` as primary key, then by the user's
  chosen sort. A gold "Featured" chip surfaces on cards.
- ~~**Sort selector on Browse Servers**~~ — shipped: client-side
  Players / Newest / Favorites chips mirroring the pack browse,
  persisted into the filter state.

## Profiles / library

- ~~**Per-file content cache GC**~~ — shipped: Settings → Cache
  section reads `cache_stats` (totals + reclaimable bytes + last
  swept) and "Clean cache" runs `cache_gc` which walks profile
  sidecars to collect referenced sha256s and removes every
  cached blob not in that set. Empty prefix dirs get swept too.
  A background sweep also runs weekly from `tauri::Builder.setup`
  (`gc_if_due`, gated on `cache_gc_state.json#lastSweepAt`).
- ~~**`-userdatafolder` profile switching**~~ — investigated;
  doesn't work for V2.6. The flag is a stock Unity arg that
  remaps `Application.persistentDataPath`, but 7DTD's binary
  doesn't use `persistentDataPath` for its userdata. It calls
  `Environment.GetFolderPath(SpecialFolder.ApplicationData)` and
  appends `7DaysToDie` directly — the Unity flag has no effect
  on that path. 7DTD doesn't parse its own `-userdatafolder=`
  arg either (literal string absent from
  `Assembly-CSharp.dll`). The internal `UserDataFolder` property
  exists with a recomputation method (`UpdateUserDataFolder-
  DependentPaths`) but has no external setter, and a leftover
  `UNUSED_UserDataFolder` field suggests the feature was
  considered and abandoned.

- **Junction-point profile switching** (the better idea) —
  instead of copying Mods/Saves/GeneratedWorlds on every
  switch, junction-link them: NTFS `mklink /J` on Windows,
  `ln -s` on macOS/Linux. Profile switch becomes "delete
  junction, recreate pointing at new profile dir" — a few
  filesystem ops instead of multi-GB copies. 7DTD's file IO
  traverses junctions transparently (Unity's `File.IO` calls
  go through the OS layer that resolves them). The profile.rs
  `replace_dir` flow we already have can swap-in a single
  reparse-point delete + create at the top level. Saves +
  GeneratedWorlds are user-editable so junction makes that
  trivial; mods are pack-managed so a stale junction during
  install is the only edge case to handle.
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
