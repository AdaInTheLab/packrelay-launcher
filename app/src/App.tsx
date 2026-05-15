import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { ask, open as openDialog } from "@tauri-apps/plugin-dialog";

import "./App.css";
import { UpdateToast } from "./UpdateToast";
import { useAutoUpdate } from "./useAutoUpdate";

// Mirrors the Rust CatalogPack struct in src-tauri/src/lib.rs.
// Tauri's invoke serializes camelCase by default since the Rust
// side annotates `#[serde(rename_all = "camelCase")]`.
type CatalogPack = {
  slug: string;
  name: string;
  summary: string | null;
  description: string | null;
  latestVersion: string | null;
  coverImage: string | null;
  publisherName: string;
  tags: string[];
  fileCount: number;
  totalSizeBytes: number;
  downloadCount: number;
  favoriteCount: number;
  // Editor's pick — admin-curated, prepends to the catalog
  // regardless of sort. Defaults to false on the Rust side via
  // #[serde(default)] so older API responses still parse.
  isFeatured: boolean;
  createdAt: string;
};

// Mirrors the Rust CatalogServer struct.
type AttachedPack = {
  slug: string;
  name: string;
  coverImage: string | null;
  latestVersion: string | null;
  attachedVersion: string | null;
};

type CatalogServer = {
  slug: string;
  name: string;
  summary: string | null;
  description: string | null;
  region: string;
  connectAddress: string | null;
  discordUrl: string | null;
  websiteUrl: string | null;
  tags: string[];
  currentPlayers: number;
  maxPlayers: number;
  lastSeenAt: string | null;
  createdAt: string;
  online: boolean;
  uptimePct: number;
  favoriteCount: number;
  attachedPack: AttachedPack | null;
};

// Top-level view the main pane renders when nothing has been
// drilled into. Overlay views (detail/install/server-detail) still
// take precedence — selecting a pack opens the detail page over
// whichever rail item is highlighted at the time.
type ViewKey =
  | "dashboard"
  | "packs"
  | "servers"
  | "library"
  | "profiles"
  | "settings";

const REGION_LABEL: Record<string, string> = {
  na_east: "NA East",
  na_west: "NA West",
  eu: "Europe",
  as: "Asia",
  oc: "Oceania",
  sa: "South America",
  af: "Africa",
};

type InstallReport = {
  displayName: string;
  version: string;
  fileCount: number;
  totalBytes: number;
  dest: string;
};

type InstallProgress = {
  bytesSoFar: number;
  totalBytes: number;
  fileCount: number;
  lastCompletedFile: string | null;
};

// Mirrors packrelay-core's UpdateReport. Same camelCase serde.
type UpdateReport = {
  displayName: string;
  fromVersion: string;
  toVersion: string;
  filesAdded: number;
  filesChanged: number;
  filesRemoved: number;
  filesKept: number;
  bytesDownloaded: number;
  dest: string;
};

// Mirrors packrelay-core auth::MeResponse + AuthState. Internally-
// tagged enum so React can switch on `kind`.
type MeResponse = {
  id: string;
  displayName: string;
  role: string;
  plan: string;
  image: string | null;
};

type AuthState =
  | { kind: "signedOut" }
  | { kind: "signedIn"; token: string; user: MeResponse };

// Which flavour of the "install" action we're rendering. Affects
// the button label, the destination input (locked for update so
// the manifest diff actually applies), and the done-state copy.
type InstallMode = "install" | "update" | "reinstall";

type DoneResult =
  | { mode: "install" | "reinstall"; report: InstallReport }
  | { mode: "update"; report: UpdateReport };

type InstallState =
  | { kind: "idle" }
  | { kind: "running"; progress: InstallProgress | null }
  | { kind: "done"; result: DoneResult }
  | { kind: "error"; message: string };

// Mirrors packrelay-core's VerifyReport / VerifyFailure / RepairReport
// (#[serde(rename_all = "camelCase")], internally-tagged enum).
type VerifyFailure =
  | { kind: "missing"; path: string }
  | { kind: "corrupt"; path: string; reason: string };

type VerifyReport = {
  displayName: string;
  version: string;
  totalFiles: number;
  failures: VerifyFailure[];
};

type RepairReport = {
  displayName: string;
  version: string;
  filesRepaired: number;
};

type RowVerifyState =
  | { kind: "idle" }
  | { kind: "verifying" }
  | { kind: "verified"; report: VerifyReport }
  | { kind: "repairing" }
  | { kind: "repaired"; report: RepairReport }
  | { kind: "error"; message: string };

// Mirrors packrelay-core::profile types verbatim. camelCase via
// #[serde(rename_all = "camelCase")] on the Rust side.
//
// Note: ProfileMeta (the create/rename return shape) is dropped on
// the frontend — every consumer refreshes the full list afterwards
// so a single-row type adds nothing.
type ProfileSummary = {
  id: string;
  name: string;
  packSlug: string | null;
  packVersion: string | null;
  createdAt: string;
  lastPlayedAt: string | null;
  isActive: boolean;
  modsBytes: number;
  savesBytes: number;
  worldsBytes: number;
  snapshotCount: number;
};

type ProfileSnapshot = {
  id: string;
  createdAt: string;
  label: string | null;
  savesBytes: number;
  worldsBytes: number;
};

type ProfileInitialState =
  | { kind: "uninitialized"; suggestedUserdataDir: string | null }
  | {
      kind: "initialized";
      activeProfileId: string | null;
      userdataDir: string | null;
    };

type UninstallFailure = { path: string; reason: string };
type UninstallReport = {
  displayName: string;
  version: string;
  filesRemoved: number;
  filesFailed: UninstallFailure[];
  sidecarRemoved: boolean;
};

type RowUninstallState =
  | { kind: "idle" }
  | { kind: "uninstalling" }
  | { kind: "partial"; report: UninstallReport }
  | { kind: "error"; message: string };

/** A row in the local install-history list. Stored in localStorage
 *  as a JSON array under HISTORY_STORAGE_KEY. */
type InstallRecord = {
  slug: string;
  name: string;
  version: string;
  dest: string;
  totalBytes: number;
  fileCount: number;
  /** ISO timestamp of when the install completed. */
  installedAt: string;
};

const HISTORY_STORAGE_KEY = "packrelay.installHistory.v1";
const HISTORY_MAX_ENTRIES = 50;

/** Browser-side fallback used if the Tauri command fails (e.g. in a
 *  Vite-only preview without the backend). The real path comes from
 *  the backend's default_install_dest() which knows the OS-canonical
 *  7DTD Mods folder location. */
const FALLBACK_DEFAULT_DEST = navigator.platform
  .toLowerCase()
  .includes("win")
  ? "C:\\Users\\You\\AppData\\Roaming\\7DaysToDie\\Mods"
  : "~/7DaysToDie/Mods";

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function formatRelativeTime(iso: string): string {
  const then = new Date(iso).getTime();
  const deltaSec = Math.max(0, (Date.now() - then) / 1000);
  if (deltaSec < 60) return "just now";
  if (deltaSec < 3600) return `${Math.floor(deltaSec / 60)}m ago`;
  if (deltaSec < 86400) return `${Math.floor(deltaSec / 3600)}h ago`;
  return `${Math.floor(deltaSec / 86400)}d ago`;
}

/** Quick-and-dirty semver compare. Splits on `.` and `-`, parses
 *  ints, lexicographic on the resulting int vectors. Doesn't fully
 *  honor SemVer 2.0's prerelease rules (alpha < beta < rc < release
 *  by string compare in practice, but PackRelay's manifest only
 *  uses pure numeric versions today, so it's fine). */
function semverIsNewer(candidate: string, baseline: string): boolean {
  const parse = (s: string): number[] =>
    s.split(/[.-]/).map((p) => {
      const n = Number.parseInt(p, 10);
      return Number.isFinite(n) ? n : 0;
    });
  const a = parse(candidate);
  const b = parse(baseline);
  const len = Math.max(a.length, b.length);
  for (let i = 0; i < len; i++) {
    const ai = a[i] ?? 0;
    const bi = b[i] ?? 0;
    if (ai > bi) return true;
    if (ai < bi) return false;
  }
  return false;
}

function loadHistory(): InstallRecord[] {
  try {
    const raw = localStorage.getItem(HISTORY_STORAGE_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed as InstallRecord[];
  } catch {
    return [];
  }
}

function saveHistory(entries: InstallRecord[]): void {
  try {
    localStorage.setItem(HISTORY_STORAGE_KEY, JSON.stringify(entries));
  } catch {
    // Storage quota or private-mode disabled. History is a nice-to-
    // have, not a critical feature — skip silently.
  }
}

// Filter state — persisted to localStorage so settings survive
// view changes AND launcher restarts. Match the website's /browse
// and /servers params so a future "open this filter in the web"
// link could share the same shape.
type PackSort = "popular" | "new" | "favorites";
type PackFilters = { q: string; tag: string; sort: PackSort };
// Server-browse sort modes mirror the cloud's /servers page so
// users moving between web and launcher see the same options:
//   players   — currentPlayers desc (busiest server first)
//   new       — createdAt desc
//   favorites — favoriteCount desc
type ServerSort = "players" | "new" | "favorites";
type ServerFilters = {
  region: string;
  onlineOnly: boolean;
  notFull: boolean;
  sort: ServerSort;
};

const PACK_FILTERS_KEY = "packrelay.packFilters.v1";
const SERVER_FILTERS_KEY = "packrelay.serverFilters.v1";

const DEFAULT_PACK_FILTERS: PackFilters = { q: "", tag: "", sort: "popular" };
const DEFAULT_SERVER_FILTERS: ServerFilters = {
  region: "",
  onlineOnly: false,
  notFull: false,
  sort: "players",
};

function loadPackFilters(): PackFilters {
  try {
    const raw = localStorage.getItem(PACK_FILTERS_KEY);
    if (!raw) return DEFAULT_PACK_FILTERS;
    const parsed = JSON.parse(raw);
    return {
      q: typeof parsed.q === "string" ? parsed.q : "",
      tag: typeof parsed.tag === "string" ? parsed.tag : "",
      sort:
        parsed.sort === "new"
          ? "new"
          : parsed.sort === "favorites"
            ? "favorites"
            : "popular",
    };
  } catch {
    return DEFAULT_PACK_FILTERS;
  }
}

function loadServerFilters(): ServerFilters {
  try {
    const raw = localStorage.getItem(SERVER_FILTERS_KEY);
    if (!raw) return DEFAULT_SERVER_FILTERS;
    const parsed = JSON.parse(raw);
    return {
      region: typeof parsed.region === "string" ? parsed.region : "",
      onlineOnly: !!parsed.onlineOnly,
      notFull: !!parsed.notFull,
      // Stored as a string but validate against the union — guards
      // against a saved value from a future version that has a
      // mode we don't recognize anymore.
      sort:
        parsed.sort === "new"
          ? "new"
          : parsed.sort === "favorites"
            ? "favorites"
            : "players",
    };
  } catch {
    return DEFAULT_SERVER_FILTERS;
  }
}

function saveFilters(key: string, value: unknown): void {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // Quota or private-mode — filter state is convenience only.
  }
}

function App() {
  const [view, setView] = useState<ViewKey>("dashboard");

  const [packs, setPacks] = useState<CatalogPack[] | null>(null);
  const [packsError, setPacksError] = useState<string | null>(null);
  const [selectedPack, setSelectedPack] = useState<CatalogPack | null>(null);
  // Whether the selected pack is currently in detail or install
  // mode. Browse cards land in "detail"; server/history shortcuts
  // skip straight to "install".
  const [packView, setPackView] = useState<"detail" | "install">("detail");
  // Whether InstallView's Back button should fall back to
  // DetailView (true, set by browse) or close the pack entirely
  // (false, set by server / history shortcuts).
  const [canReturnToDetail, setCanReturnToDetail] = useState(false);

  const [servers, setServers] = useState<CatalogServer[] | null>(null);
  const [serversError, setServersError] = useState<string | null>(null);
  const [selectedServer, setSelectedServer] = useState<CatalogServer | null>(
    null
  );

  const [history, setHistory] = useState<InstallRecord[]>(() => loadHistory());
  // OS-canonical 7DTD Mods/ path, fetched once on mount. The
  // InstallView uses it as the initial destination so users don't
  // type or even pick a folder for the common case.
  const [defaultDest, setDefaultDest] = useState<string>(FALLBACK_DEFAULT_DEST);

  // Filter state hoisted out of the views so it survives both
  // intra-app navigation (clicking a pack and coming back) and
  // launcher restarts. Persisted via the effects below.
  const [packFilters, setPackFilters] = useState<PackFilters>(() =>
    loadPackFilters()
  );
  const [serverFilters, setServerFilters] = useState<ServerFilters>(() =>
    loadServerFilters()
  );
  useEffect(() => {
    saveFilters(PACK_FILTERS_KEY, packFilters);
  }, [packFilters]);
  useEffect(() => {
    saveFilters(SERVER_FILTERS_KEY, serverFilters);
  }, [serverFilters]);

  // Auth state — resolved on startup from the persisted token (if
  // any). When signedIn the Header chip shows the user's name; the
  // SignInModal opens via setShowSignIn(true) from the chip click
  // when out, or from the menu when in.
  const [auth, setAuth] = useState<AuthState>({ kind: "signedOut" });
  const [showSignIn, setShowSignIn] = useState(false);

  // Favorites: two Sets of slugs the signed-in user has hearted.
  // Sourced from /api/v1/me/favorites, refreshed after every
  // toggle so the heart state stays in sync across views without
  // per-card auth roundtrips. Null when we haven't fetched yet
  // (uninitialized vs "empty set" — matters for first-paint).
  const [packFavs, setPackFavs] = useState<Set<string> | null>(null);
  const [serverFavs, setServerFavs] = useState<Set<string> | null>(null);

  const refreshFavorites = useCallback(async () => {
    try {
      const f = await invoke<{ packs: string[]; servers: string[] }>(
        "fetch_my_favorites"
      );
      setPackFavs(new Set(f.packs));
      setServerFavs(new Set(f.servers));
    } catch {
      // Signed out, network blip, etc — leave state as-is. UI
      // gracefully degrades to "no filled hearts" rather than
      // surfacing an error banner for a non-essential signal.
    }
  }, []);

  // Toggle a single pack heart, optimistically updating local
  // state and rolling back if the network call fails. Bumps the
  // catalog's favoriteCount in place too so cards re-render
  // without a full list_packs refetch.
  const togglePackFavorite = useCallback(async (slug: string) => {
    let previous: { wasFavorited: boolean; previousCount: number } | null =
      null;
    setPackFavs((curr) => {
      const next = new Set(curr ?? []);
      previous = {
        wasFavorited: next.has(slug),
        previousCount: 0, // populated below from packs state
      };
      if (next.has(slug)) next.delete(slug);
      else next.add(slug);
      return next;
    });
    setPacks((curr) => {
      if (!curr) return curr;
      return curr.map((p) => {
        if (p.slug !== slug) return p;
        if (previous) previous.previousCount = p.favoriteCount;
        return {
          ...p,
          favoriteCount: previous?.wasFavorited
            ? Math.max(0, p.favoriteCount - 1)
            : p.favoriteCount + 1,
        };
      });
    });
    try {
      const result = await invoke<{ favorited: boolean; count: number }>(
        "toggle_pack_favorite",
        { slug }
      );
      // Reconcile against the authoritative server count in case
      // we and the server disagreed (e.g. parallel toggles from
      // another device).
      setPacks((curr) =>
        curr
          ? curr.map((p) =>
              p.slug === slug ? { ...p, favoriteCount: result.count } : p
            )
          : curr
      );
      setPackFavs((curr) => {
        const next = new Set(curr ?? []);
        if (result.favorited) next.add(slug);
        else next.delete(slug);
        return next;
      });
    } catch {
      // Roll back the optimistic update.
      if (previous) {
        const prev: { wasFavorited: boolean; previousCount: number } =
          previous;
        setPackFavs((curr) => {
          const next = new Set(curr ?? []);
          if (prev.wasFavorited) next.add(slug);
          else next.delete(slug);
          return next;
        });
        setPacks((curr) =>
          curr
            ? curr.map((p) =>
                p.slug === slug ? { ...p, favoriteCount: prev.previousCount } : p
              )
            : curr
        );
      }
    }
  }, []);

  // Same shape for servers.
  const toggleServerFavorite = useCallback(async (slug: string) => {
    let previous: { wasFavorited: boolean; previousCount: number } | null =
      null;
    setServerFavs((curr) => {
      const next = new Set(curr ?? []);
      previous = { wasFavorited: next.has(slug), previousCount: 0 };
      if (next.has(slug)) next.delete(slug);
      else next.add(slug);
      return next;
    });
    setServers((curr) => {
      if (!curr) return curr;
      return curr.map((s) => {
        if (s.slug !== slug) return s;
        if (previous) previous.previousCount = s.favoriteCount;
        return {
          ...s,
          favoriteCount: previous?.wasFavorited
            ? Math.max(0, s.favoriteCount - 1)
            : s.favoriteCount + 1,
        };
      });
    });
    try {
      const result = await invoke<{ favorited: boolean; count: number }>(
        "toggle_server_favorite",
        { slug }
      );
      setServers((curr) =>
        curr
          ? curr.map((s) =>
              s.slug === slug ? { ...s, favoriteCount: result.count } : s
            )
          : curr
      );
      setServerFavs((curr) => {
        const next = new Set(curr ?? []);
        if (result.favorited) next.add(slug);
        else next.delete(slug);
        return next;
      });
    } catch {
      if (previous) {
        const prev: { wasFavorited: boolean; previousCount: number } =
          previous;
        setServerFavs((curr) => {
          const next = new Set(curr ?? []);
          if (prev.wasFavorited) next.add(slug);
          else next.delete(slug);
          return next;
        });
        setServers((curr) =>
          curr
            ? curr.map((s) =>
                s.slug === slug ? { ...s, favoriteCount: prev.previousCount } : s
              )
            : curr
        );
      }
    }
  }, []);

  useEffect(() => {
    (async () => {
      try {
        const list = await invoke<CatalogPack[]>("list_packs");
        setPacks(list);
      } catch (e) {
        setPacksError(typeof e === "string" ? e : `${e}`);
      }
    })();
    (async () => {
      try {
        const list = await invoke<CatalogServer[]>("list_servers");
        setServers(list);
      } catch (e) {
        setServersError(typeof e === "string" ? e : `${e}`);
      }
    })();
    (async () => {
      try {
        const detected = await invoke<string | null>("default_install_dest");
        if (detected) setDefaultDest(detected);
      } catch {
        // Leave the fallback in place — used in Vite-only preview
        // mode and on platforms without a canonical path.
      }
    })();
    (async () => {
      try {
        const state = await invoke<AuthState>("get_auth_state");
        setAuth(state);
        // If we're signed in on boot, prefetch the favorites
        // set so hearts render filled immediately on first paint.
        if (state.kind === "signedIn") void refreshFavorites();
      } catch {
        // Token validation network error etc — stay signed out;
        // the user can re-paste when they want.
      }
    })();
  }, [refreshFavorites]);

  // After sign-in flow lands, refresh favorites so the cards
  // start rendering filled hearts without waiting for a manual
  // page refresh.
  useEffect(() => {
    if (auth.kind === "signedIn") {
      void refreshFavorites();
    } else {
      setPackFavs(null);
      setServerFavs(null);
    }
  }, [auth.kind, refreshFavorites]);

  const handleSignOut = useCallback(async () => {
    try {
      const next = await invoke<AuthState>("sign_out");
      setAuth(next);
    } catch {
      // Local-only operation; failure here is exceedingly rare
      // (filesystem write). Leaving the chip as-is is acceptable.
    }
  }, []);

  const handleViewChange = useCallback((next: ViewKey) => {
    setView(next);
    // Switching views clears any drill-down overlay so the user
    // always lands on the rail item they picked, not on whatever
    // pack/server was previously open.
    setSelectedPack(null);
    setSelectedServer(null);
    setPackView("detail");
    setCanReturnToDetail(false);
  }, []);

  // From browse: card click opens the rich detail page; Install is
  // a second click. This is the route the launcher's pitch surfaces.
  const openPackDetail = useCallback((pack: CatalogPack) => {
    setSelectedPack(pack);
    setPackView("detail");
    setCanReturnToDetail(true);
  }, []);

  // From server/history: skip detail and go straight to install. The
  // user came in with intent — server-detail already shows the
  // context, history rows are by definition things you've installed
  // before. canReturnToDetail stays false so InstallView's Back
  // closes the pack rather than rebounding through DetailView.
  const openPackInstall = useCallback((pack: CatalogPack) => {
    setSelectedPack(pack);
    setPackView("install");
    setCanReturnToDetail(false);
  }, []);

  const recordInstall = useCallback(
    (slug: string, report: InstallReport) => {
      const entry: InstallRecord = {
        slug,
        name: report.displayName,
        version: report.version,
        dest: report.dest,
        totalBytes: report.totalBytes,
        fileCount: report.fileCount,
        installedAt: new Date().toISOString(),
      };
      setHistory((prev) => {
        // Dedup: drop any prior entry for the same slug — the latest
        // install of a pack is what matters; older entries for the
        // same slug just clutter the sidebar.
        const next = [entry, ...prev.filter((r) => r.slug !== slug)];
        const trimmed = next.slice(0, HISTORY_MAX_ENTRIES);
        saveHistory(trimmed);
        return trimmed;
      });
    },
    []
  );

  // Called after a successful smart update. Bumps the existing
  // history record's version + timestamp in place; we deliberately
  // don't dedupe-to-top here because the sort order already reflects
  // recency, and re-installing keeps the same slug→one-row invariant.
  const recordUpdate = useCallback(
    (slug: string, report: UpdateReport) => {
      setHistory((prev) => {
        const next = prev.map((r) =>
          r.slug === slug
            ? {
                ...r,
                version: report.toVersion,
                installedAt: new Date().toISOString(),
                // fileCount/totalBytes aren't reported by update —
                // we don't bother stale-stamping them; the values
                // are only used for the recent-installs sidebar
                // hover stats today.
              }
            : r
        );
        // Float the updated entry to the top so it reads as a fresh
        // event, like a re-install would.
        const updated = next.find((r) => r.slug === slug);
        const rest = next.filter((r) => r.slug !== slug);
        const reordered = updated ? [updated, ...rest] : next;
        saveHistory(reordered);
        return reordered;
      });
    },
    []
  );

  // Called after a successful uninstall — drops the record from
  // the sidebar. Keyed by slug+installedAt so re-installing the
  // same slug later doesn't accidentally match a stale entry.
  const removeFromHistory = useCallback((record: InstallRecord) => {
    setHistory((prev) => {
      const next = prev.filter(
        (r) => !(r.slug === record.slug && r.installedAt === record.installedAt)
      );
      saveHistory(next);
      return next;
    });
  }, []);

  const reinstallFromHistory = useCallback(
    (record: InstallRecord) => {
      // If the catalog has a matching pack, drill into InstallView
      // with it pre-selected. Otherwise the pack was removed since;
      // surface a soft error by ignoring the click.
      const match = packs?.find((p) => p.slug === record.slug);
      if (match) {
        // Library is the natural "home" for a re-install action; if
        // the user got here from another rail item, send them back to
        // their list afterward via the back button.
        setView("library");
        setSelectedServer(null);
        openPackInstall(match);
      }
    },
    [packs, openPackInstall]
  );

  // Pre-compute the slug → latest catalog version map once per
  // render; the sidebar uses it to flag "update available" entries
  // without doing a linear scan per row.
  const latestVersionBySlug = new Map<string, string>(
    (packs ?? [])
      .filter((p): p is CatalogPack & { latestVersion: string } =>
        Boolean(p.latestVersion)
      )
      .map((p) => [p.slug, p.latestVersion])
  );

  // Lookup table: server attached-pack slug → full CatalogPack.
  // Used when the user clicks "Install pack" on a server detail
  // page — we need the full pack record (file count, total size,
  // cover) to drive InstallView even though the server endpoint
  // only embeds a thin AttachedPack reference.
  const packBySlug = new Map<string, CatalogPack>(
    (packs ?? []).map((p) => [p.slug, p])
  );

  /** What's rendered in the main pane. Precedence:
   *  1. selectedPack + packView === "install" → InstallView
   *  2. selectedPack + packView === "detail"  → DetailView
   *  3. selectedServer → ServerDetailView
   *  4. active tab's browse
   */
  let mainContent: React.ReactNode;
  if (selectedPack) {
    const backToServer = selectedServer ?? null;
    // If the selected pack is in history we're either updating (catalog
    // version is newer) or re-installing (same/older). Drives the
    // InstallView's mode + locks dest to the on-disk path so the
    // smart-update manifest diff actually applies.
    const installedRecord =
      history.find((r) => r.slug === selectedPack.slug) ?? null;
    if (packView === "install") {
      mainContent = (
        <InstallView
          pack={selectedPack}
          defaultDest={installedRecord?.dest ?? defaultDest}
          installedRecord={installedRecord}
          connectAddress={backToServer?.connectAddress ?? null}
          backLabel={
            backToServer
              ? backToServer.name.toUpperCase()
              : canReturnToDetail
                ? "DETAILS"
                : "BROWSE"
          }
          onBack={() => {
            if (canReturnToDetail) {
              setPackView("detail");
            } else {
              setSelectedPack(null);
            }
          }}
          onInstalled={(report) => recordInstall(selectedPack.slug, report)}
          onUpdated={(report) => recordUpdate(selectedPack.slug, report)}
        />
      );
    } else {
      mainContent = (
        <DetailView
          pack={selectedPack}
          installedRecord={installedRecord}
          favorited={packFavs?.has(selectedPack.slug) ?? false}
          signedIn={auth.kind === "signedIn"}
          onToggleFavorite={() => togglePackFavorite(selectedPack.slug)}
          onSignInRequest={() => setShowSignIn(true)}
          onBack={() => setSelectedPack(null)}
          onInstall={() => setPackView("install")}
        />
      );
    }
  } else if (selectedServer) {
    // Look up the attached pack's install state so the detail
    // view can swap between "Install" / "Update" / "Connect"
    // depending on what's already on disk vs what the server's
    // pinned to.
    const attachedSlug = selectedServer.attachedPack?.slug;
    const attachedInstalled =
      attachedSlug != null
        ? history.find((r) => r.slug === attachedSlug) ?? null
        : null;
    mainContent = (
      <ServerDetailView
        server={selectedServer}
        attachedPack={
          selectedServer.attachedPack
            ? packBySlug.get(selectedServer.attachedPack.slug) ?? null
            : null
        }
        installedRecord={attachedInstalled}
        favorited={serverFavs?.has(selectedServer.slug) ?? false}
        signedIn={auth.kind === "signedIn"}
        onToggleFavorite={() => toggleServerFavorite(selectedServer.slug)}
        onSignInRequest={() => setShowSignIn(true)}
        onBack={() => setSelectedServer(null)}
        onInstall={(pack) => openPackInstall(pack)}
      />
    );
  } else if (view === "packs") {
    mainContent = (
      <BrowseView
        packs={packs}
        error={packsError}
        onSelect={openPackDetail}
        filters={packFilters}
        onFiltersChange={setPackFilters}
      />
    );
  } else if (view === "servers") {
    mainContent = (
      <ServerBrowseView
        servers={servers}
        error={serversError}
        onSelect={setSelectedServer}
        filters={serverFilters}
        onFiltersChange={setServerFilters}
      />
    );
  } else if (view === "library") {
    mainContent = (
      <LibraryView
        history={history}
        packBySlug={packBySlug}
        latestVersionBySlug={latestVersionBySlug}
        catalogReady={packs !== null}
        onPick={reinstallFromHistory}
        onRemove={removeFromHistory}
        onBrowse={() => setView("packs")}
      />
    );
  } else if (view === "profiles") {
    mainContent = <ProfilesView />;
  } else if (view === "settings") {
    mainContent = <SettingsView auth={auth} onSignOut={handleSignOut} />;
  } else {
    mainContent = (
      <DashboardView
        auth={auth}
        history={history}
        packBySlug={packBySlug}
        onBrowse={() => setView("packs")}
        onOpenPack={openPackDetail}
        onSignIn={() => setShowSignIn(true)}
      />
    );
  }

  return (
    <div className="min-h-screen flex flex-col">
      <div className="flex-1 flex min-h-0">
        <LeftRail
          view={view}
          onViewChange={handleViewChange}
          auth={auth}
          onSignInClick={() => setShowSignIn(true)}
          onSignOut={handleSignOut}
        />
        <main className="flex-1 overflow-y-auto">{mainContent}</main>
      </div>
      <FooterStrip />
      {showSignIn && (
        <SignInModal
          onClose={() => setShowSignIn(false)}
          onSignedIn={(next) => {
            setAuth(next);
            setShowSignIn(false);
          }}
        />
      )}
      {/* Background-checks for a new launcher release on mount;
        * renders nothing when nothing's available. See useAutoUpdate. */}
      <AutoUpdateDock />
    </div>
  );
}

/**
 * Wraps the update hook + toast so the App-level component
 * tree stays tidy. Pulled out so the hook can be lifted later
 * (e.g. when we want to expose a manual "Check for updates"
 * button in Settings) without re-plumbing call sites.
 */
function AutoUpdateDock() {
  const updater = useAutoUpdate();
  return <UpdateToast {...updater} />;
}

// Full-height left-rail nav. Replaces the previous top-tab header;
// primary nav lives here, the auth chip docks at the bottom, and
// community links sit in their own subsection. Active state is
// driven from App's ViewKey state machine — overlays (detail /
// install / server-detail) leave the rail untouched so the user
// can always see "where they were" in the structure.
function LeftRail({
  view,
  onViewChange,
  auth,
  onSignInClick,
  onSignOut,
}: {
  view: ViewKey;
  onViewChange: (next: ViewKey) => void;
  auth: AuthState;
  onSignInClick: () => void;
  onSignOut: () => void;
}) {
  return (
    <aside className="w-56 shrink-0 border-r border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/40 flex flex-col">
      <div className="px-4 pt-4 pb-3 border-b border-[var(--color-bg-raised)]">
        {/* Brand mark — served from app/public/ so Vite copies it as
            a static asset in both dev and build. Designed for dark
            backgrounds so it sits cleanly on the rail's panel bg. */}
        <img
          src="/dark-logo-trans.png"
          alt="PackRelay.cloud"
          className="w-full h-auto select-none"
          draggable={false}
        />
        <div className="text-[9px] tracking-[0.22em] uppercase text-[var(--color-text-dim)] text-center mt-1">
          launcher · v0.1
        </div>
      </div>

      <nav className="flex-1 overflow-y-auto px-2 py-3 space-y-0.5">
        <RailItem
          active={view === "dashboard"}
          onClick={() => onViewChange("dashboard")}
          glyph={<HomeGlyph />}
        >
          Dashboard
        </RailItem>
        <RailItem
          active={view === "packs"}
          onClick={() => onViewChange("packs")}
          glyph={<PackGlyph />}
        >
          Browse Packs
        </RailItem>
        <RailItem
          active={view === "servers"}
          onClick={() => onViewChange("servers")}
          glyph={<ServerGlyph />}
        >
          Browse Servers
        </RailItem>
        <RailItem
          active={view === "library"}
          onClick={() => onViewChange("library")}
          glyph={<LibraryGlyph />}
        >
          My Library
        </RailItem>
        <RailItem
          active={view === "profiles"}
          onClick={() => onViewChange("profiles")}
          glyph={<ProfileGlyph />}
        >
          Profiles
        </RailItem>
        <RailItem
          active={view === "settings"}
          onClick={() => onViewChange("settings")}
          glyph={<SettingsGlyph />}
        >
          Settings
        </RailItem>

        <div className="px-3 pt-5 pb-1 text-[9px] font-medium tracking-[0.22em] uppercase text-[var(--color-text-dim)]/70">
          Community
        </div>
        <RailExternal
          href="https://packrelay.cloud/discord"
          glyph={<DiscordGlyph />}
        >
          Discord
        </RailExternal>
        <RailExternal
          href="https://packrelay.cloud/news"
          glyph={<NewsGlyph />}
        >
          News
        </RailExternal>
        <RailExternal
          href="https://packrelay.cloud/support"
          glyph={<SupportGlyph />}
        >
          Support
        </RailExternal>
      </nav>

      <div className="px-3 pb-3 pt-2 border-t border-[var(--color-bg-raised)]">
        <AuthChip
          auth={auth}
          onSignInClick={onSignInClick}
          onSignOut={onSignOut}
        />
      </div>
    </aside>
  );
}

// One row in the rail's primary nav. Active = accent-tinted bg +
// soft glow on the left edge; inactive = subdued text that brightens
// on hover. The leading glyph sits at fixed size so labels align.
function RailItem({
  active,
  onClick,
  glyph,
  children,
}: {
  active: boolean;
  onClick: () => void;
  glyph: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`w-full flex items-center gap-2.5 px-3 py-2 rounded-md text-[12px] tracking-wide transition-colors ${
        active
          ? "bg-[var(--color-accent)]/15 text-[var(--color-accent-soft)] ring-1 ring-[var(--color-accent-soft)]/30"
          : "text-[var(--color-text-bright)]/85 hover:text-[var(--color-text-bright)] hover:bg-[var(--color-bg-raised)]/40"
      }`}
    >
      <span
        className={`size-4 shrink-0 ${
          active
            ? "text-[var(--color-accent-soft)]"
            : "text-[var(--color-text-dim)]"
        }`}
      >
        {glyph}
      </span>
      <span className="font-medium">{children}</span>
    </button>
  );
}

// Same shape as RailItem, but for community links that open in the
// user's browser via the opener plugin. Never shows "active" since
// it's not a launcher route.
function RailExternal({
  href,
  glyph,
  children,
}: {
  href: string;
  glyph: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={async () => {
        try {
          const mod = await import("@tauri-apps/plugin-opener");
          await mod.openUrl(href);
        } catch {
          // No graceful fallback inside the rail — community links
          // are non-essential and the user can copy the URL via the
          // packrelay.cloud header if the opener plugin glue is
          // somehow missing.
        }
      }}
      className="w-full flex items-center gap-2.5 px-3 py-2 rounded-md text-[12px] tracking-wide text-[var(--color-text-bright)]/85 hover:text-[var(--color-text-bright)] hover:bg-[var(--color-bg-raised)]/40 transition-colors"
    >
      <span className="size-4 shrink-0 text-[var(--color-text-dim)]">
        {glyph}
      </span>
      <span className="font-medium flex-1 text-left">{children}</span>
      <span className="text-[10px] text-[var(--color-text-dim)]">↗</span>
    </button>
  );
}

// Auth chip — rail-bottom shape this time. Same two states
// (signedOut → "Sign in" pill, signedIn → avatar + name with a
// click-toggled menu) but laid out for the narrow rail column.
function AuthChip({
  auth,
  onSignInClick,
  onSignOut,
}: {
  auth: AuthState;
  onSignInClick: () => void;
  onSignOut: () => void;
}) {
  const [menuOpen, setMenuOpen] = useState(false);

  if (auth.kind === "signedOut") {
    return (
      <button
        type="button"
        onClick={onSignInClick}
        className="inline-flex items-center gap-1.5 px-3 py-1.5 rounded-md border border-[var(--color-bg-raised)] hover:border-[var(--color-accent-soft)]/40 hover:text-[var(--color-text-bright)] text-[var(--color-text-bright)]/85 text-[11px] tracking-[0.14em] uppercase transition-colors"
      >
        Sign in
      </button>
    );
  }

  const initial = auth.user.displayName.slice(0, 1).toUpperCase();
  return (
    <div className="relative">
      <button
        type="button"
        onClick={() => setMenuOpen((v) => !v)}
        className="w-full flex items-center gap-2 px-2 py-1.5 rounded-md hover:bg-[var(--color-bg-raised)]/50 transition-colors"
      >
        <span className="size-7 rounded-full bg-[var(--color-accent)]/30 text-[var(--color-accent-soft)] text-[12px] font-semibold flex items-center justify-center overflow-hidden shrink-0">
          {auth.user.image ? (
            <img
              src={auth.user.image}
              alt=""
              className="w-full h-full object-cover"
            />
          ) : (
            initial
          )}
        </span>
        <span className="min-w-0 flex-1 text-left">
          <span className="block text-[12px] font-medium text-[var(--color-text-bright)] truncate">
            {auth.user.displayName}
          </span>
          <span className="block text-[9px] uppercase tracking-[0.16em] text-[var(--color-text-dim)] truncate">
            {auth.user.role.replace("_", " ")}
          </span>
        </span>
        <span className="text-[10px] text-[var(--color-text-dim)] shrink-0">
          {menuOpen ? "▴" : "▾"}
        </span>
      </button>
      {menuOpen && (
        <>
          {/* Click-away catcher so the menu closes on any outside click */}
          <div
            className="fixed inset-0 z-40"
            onClick={() => setMenuOpen(false)}
          />
          {/* Open upward — the chip is at the rail-bottom, so a
              downward menu would overflow off-screen. Aligned to the
              chip's left edge for the same reason. */}
          <div className="absolute left-0 right-0 bottom-full mb-1 z-50 rounded-md border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)] shadow-lg overflow-hidden">
            <div className="px-3 py-2 border-b border-[var(--color-bg-raised)]/60">
              <div className="text-[11px] text-[var(--color-text-bright)] font-medium truncate">
                {auth.user.displayName}
              </div>
              <div className="text-[10px] text-[var(--color-text-dim)] uppercase tracking-wide mt-0.5">
                {auth.user.role.replace("_", " ")} · {auth.user.plan}
              </div>
            </div>
            <button
              type="button"
              onClick={() => {
                setMenuOpen(false);
                onSignOut();
              }}
              className="w-full text-left px-3 py-2 text-[11px] tracking-[0.14em] uppercase text-[var(--color-text-dim)] hover:text-[var(--color-status-danger)] hover:bg-[var(--color-bg-raised)]/40 transition-colors"
            >
              Sign out
            </button>
          </div>
        </>
      )}
    </div>
  );
}

// Modal that drives the launcher's sign-in. Two steps in a single
// dialog: a "go mint a token" link out to packrelay.cloud, then a
// paste field that invokes sign_in. The Rust side handles validation
// + storage; we just relay the result.
function SignInModal({
  onClose,
  onSignedIn,
}: {
  onClose: () => void;
  onSignedIn: (next: AuthState) => void;
}) {
  const [token, setToken] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const openTokensPage = useCallback(async () => {
    try {
      // Frontend opener works fine for https:// — the default
      // capability scope covers web URLs without extra config.
      // We dynamically import so the bundle doesn't ship the plugin
      // glue at module-load time.
      const mod = await import("@tauri-apps/plugin-opener");
      await mod.openUrl("https://packrelay.cloud/account/tokens");
    } catch {
      setError(
        "Couldn't open packrelay.cloud — copy this URL into your browser instead: https://packrelay.cloud/account/tokens"
      );
    }
  }, []);

  const submit = useCallback(async () => {
    setError(null);
    setSubmitting(true);
    try {
      const next = await invoke<AuthState>("sign_in", { token });
      onSignedIn(next);
    } catch (e) {
      setError(typeof e === "string" ? e : `${e}`);
    } finally {
      setSubmitting(false);
    }
  }, [token, onSignedIn]);

  return (
    <div
      className="fixed inset-0 z-50 bg-black/60 backdrop-blur-sm flex items-center justify-center px-6"
      onClick={onClose}
    >
      <div
        className="w-full max-w-md rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)] shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="px-5 py-4 border-b border-[var(--color-bg-raised)]/60 flex items-center justify-between">
          <h2 className="text-sm font-semibold text-[var(--color-text-bright)]">
            Sign in to PackRelay
          </h2>
          <button
            type="button"
            onClick={onClose}
            className="text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)] text-lg leading-none"
            aria-label="Close"
          >
            ×
          </button>
        </div>
        <div className="px-5 py-5 space-y-4">
          <ol className="text-[11px] text-[var(--color-text-dim)] leading-relaxed space-y-1.5 list-decimal list-inside">
            <li>
              Open your{" "}
              <button
                type="button"
                onClick={openTokensPage}
                className="text-[var(--color-accent-soft)] hover:underline inline"
              >
                API tokens page
              </button>{" "}
              on packrelay.cloud.
            </li>
            <li>Mint a token named e.g. &ldquo;Launcher — laptop&rdquo;.</li>
            <li>Paste it below.</li>
          </ol>
          <div>
            <label
              htmlFor="auth-token"
              className="block text-[10px] font-medium tracking-[0.14em] uppercase text-[var(--color-text-bright)]/85 mb-1.5"
            >
              API token
            </label>
            <input
              id="auth-token"
              type="password"
              value={token}
              onChange={(e) => setToken(e.currentTarget.value)}
              placeholder="pr_……"
              autoComplete="off"
              spellCheck={false}
              disabled={submitting}
              className="w-full rounded-md bg-[var(--color-bg-page)] border border-[var(--color-bg-raised)] px-3 py-2 text-sm font-mono text-[var(--color-text-bright)] outline-none focus:border-[var(--color-accent-soft)]/60 focus:ring-2 focus:ring-[var(--color-accent)]/20 transition-colors disabled:opacity-60"
              onKeyDown={(e) => {
                if (e.key === "Enter" && token.trim() && !submitting) {
                  void submit();
                }
              }}
            />
          </div>
          {error && (
            <div className="rounded-md border border-[var(--color-status-danger)]/40 bg-[var(--color-status-danger)]/10 px-3 py-2 text-[11px] text-[var(--color-status-danger)] break-words">
              {error}
            </div>
          )}
          <div className="flex justify-end gap-2 pt-1">
            <button
              type="button"
              onClick={onClose}
              disabled={submitting}
              className="inline-flex items-center px-3 py-1.5 rounded-md border border-[var(--color-bg-raised)] hover:border-[var(--color-accent-soft)]/40 hover:text-[var(--color-text-bright)] text-[var(--color-text-bright)]/85 text-[11px] tracking-[0.14em] uppercase transition-colors disabled:opacity-50"
            >
              Cancel
            </button>
            <button
              type="button"
              onClick={submit}
              disabled={submitting || !token.trim()}
              className="inline-flex items-center px-4 py-1.5 rounded-md bg-[var(--color-accent)] hover:bg-[var(--color-accent)]/90 disabled:opacity-50 disabled:cursor-not-allowed transition-colors text-[11px] tracking-[0.14em] uppercase font-medium"
            >
              {submitting ? "Checking…" : "Sign in"}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

// Main-pane home for the user's installed packs. Replaces the old
// sidebar list. Renders a grid of cover-art tiles, an empty state
// that points to Browse, and (via LibraryTile) the same verify /
// uninstall affordances the sidebar row had — just in a tile-shaped
// visual instead of a tight row.
function LibraryView({
  history,
  packBySlug,
  latestVersionBySlug,
  catalogReady,
  onPick,
  onRemove,
  onBrowse,
}: {
  history: InstallRecord[];
  packBySlug: Map<string, CatalogPack>;
  latestVersionBySlug: Map<string, string>;
  catalogReady: boolean;
  onPick: (r: InstallRecord) => void;
  onRemove: (r: InstallRecord) => void;
  onBrowse: () => void;
}) {
  if (history.length === 0) {
    return (
      <div className="px-6 py-12">
        <div className="mb-6">
          <h1 className="text-xl font-semibold tracking-tight">My Library</h1>
          <p className="text-xs text-[var(--color-text-dim)] mt-1">
            Packs you install land here.
          </p>
        </div>
        <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 px-6 py-10 text-center max-w-xl">
          <div className="text-sm text-[var(--color-text-bright)] font-medium mb-1">
            No installs yet
          </div>
          <p className="text-xs text-[var(--color-text-dim)] mb-4">
            Browse the catalog and install a pack to fill this view.
          </p>
          <button
            type="button"
            onClick={onBrowse}
            className="inline-flex items-center px-4 py-2 rounded-md bg-[var(--color-accent)] hover:bg-[var(--color-accent)]/90 text-xs font-medium transition-colors"
          >
            Browse packs
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="px-6 py-8">
      <div className="mb-5 flex items-end justify-between gap-4 flex-wrap">
        <div>
          <h1 className="text-xl font-semibold tracking-tight">My Library</h1>
          <p className="text-xs text-[var(--color-text-dim)] mt-1">
            Packs you&apos;ve installed on this machine. Click one to re-
            install, update, or grab a connect address.
          </p>
        </div>
        <div className="text-[11px] text-[var(--color-text-dim)] tabular-nums">
          {history.length} pack{history.length === 1 ? "" : "s"}
        </div>
      </div>
      <ul className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4">
        {history.map((r) => (
          <LibraryTile
            key={`${r.slug}-${r.installedAt}`}
            record={r}
            catalogPack={packBySlug.get(r.slug) ?? null}
            latestVersionBySlug={latestVersionBySlug}
            catalogReady={catalogReady}
            onPick={onPick}
            onRemove={onRemove}
          />
        ))}
      </ul>
    </div>
  );
}

// One tile in the library grid. Click the card → re-install /
// update / detail (depending on InstallView's mode); the small
// ✓ and ✕ affordances in the top-right run verify and uninstall
// without taking you off the page. The same state machine the
// old sidebar row had, just laid out for a wider card with cover
// art on top.
function LibraryTile({
  record,
  catalogPack,
  latestVersionBySlug,
  catalogReady,
  onPick,
  onRemove,
}: {
  record: InstallRecord;
  catalogPack: CatalogPack | null;
  latestVersionBySlug: Map<string, string>;
  catalogReady: boolean;
  onPick: (r: InstallRecord) => void;
  onRemove: (r: InstallRecord) => void;
}) {
  const [verify, setVerify] = useState<RowVerifyState>({ kind: "idle" });
  const [uninstall, setUninstall] = useState<RowUninstallState>({
    kind: "idle",
  });

  const latest = latestVersionBySlug.get(record.slug);
  const updateAvailable =
    latest !== undefined && semverIsNewer(latest, record.version);

  const runVerify = useCallback(async () => {
    setVerify({ kind: "verifying" });
    try {
      const report = await invoke<VerifyReport>("verify_pack", {
        dest: record.dest,
      });
      setVerify({ kind: "verified", report });
    } catch (e) {
      setVerify({ kind: "error", message: typeof e === "string" ? e : `${e}` });
    }
  }, [record.dest]);

  const runRepair = useCallback(async () => {
    setVerify({ kind: "repairing" });
    try {
      const report = await invoke<RepairReport>("repair_pack", {
        dest: record.dest,
      });
      setVerify({ kind: "repaired", report });
    } catch (e) {
      setVerify({ kind: "error", message: typeof e === "string" ? e : `${e}` });
    }
  }, [record.dest]);

  const runUninstall = useCallback(async () => {
    const ok = await ask(
      `Remove ${record.name} v${record.version}?\n\nDeletes the pack's files under ${record.dest}. The Mods/ folder and any other packs inside it are left alone.`,
      { title: "Uninstall pack", kind: "warning" }
    );
    if (!ok) return;
    setUninstall({ kind: "uninstalling" });
    setVerify({ kind: "idle" });
    try {
      const report = await invoke<UninstallReport>("uninstall_pack", {
        dest: record.dest,
      });
      if (report.filesFailed.length === 0) {
        onRemove(record);
      } else {
        setUninstall({ kind: "partial", report });
      }
    } catch (e) {
      setUninstall({
        kind: "error",
        message: typeof e === "string" ? e : `${e}`,
      });
    }
  }, [record, onRemove]);

  const inFlight =
    verify.kind === "verifying" ||
    verify.kind === "repairing" ||
    uninstall.kind === "uninstalling";

  return (
    <li className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)] overflow-hidden hover:border-[var(--color-accent-soft)]/40 transition-colors group">
      <button
        type="button"
        onClick={() => onPick(record)}
        disabled={!catalogReady}
        className="w-full text-left disabled:opacity-60 disabled:cursor-not-allowed"
      >
        <div className="aspect-[16/9] bg-[var(--color-bg-raised)] relative overflow-hidden">
          <div className="absolute inset-0 flex items-center justify-center text-[var(--color-text-dim)]/50 text-xs tracking-[0.2em] uppercase">
            no cover
          </div>
          <CoverImage
            src={catalogPack?.coverImage}
            className="absolute inset-0 w-full h-full object-cover group-hover:scale-105 transition-transform duration-500"
          />
          <div className="absolute inset-0 bg-gradient-to-t from-[var(--color-bg-panel)] via-transparent to-transparent" />
          {updateAvailable && (
            <span
              title={`Update available: v${latest}`}
              className="absolute top-2 left-2 inline-flex items-center gap-0.5 px-2 py-0.5 rounded text-[10px] tracking-wide uppercase font-medium bg-[var(--color-accent)]/20 text-[var(--color-accent-soft)] ring-1 ring-[var(--color-accent-soft)]/40 backdrop-blur-sm"
            >
              ↻ Update
            </span>
          )}
          {/* Inline action chips, top-right. role="button"-on-span +
              stopPropagation so the outer click-to-pick doesn't fire
              when the user wants to verify or uninstall. */}
          <div className="absolute top-2 right-2 flex gap-1">
            <span
              role="button"
              tabIndex={0}
              title="Verify installed files"
              onClick={(e) => {
                e.stopPropagation();
                if (!inFlight) runVerify();
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  e.stopPropagation();
                  if (!inFlight) runVerify();
                }
              }}
              className="inline-flex items-center px-1.5 py-0.5 rounded text-[10px] uppercase tracking-wide bg-[var(--color-bg-page)]/80 backdrop-blur-sm text-[var(--color-text-bright)]/85 ring-1 ring-[var(--color-bg-raised)]/40 hover:text-[var(--color-accent-soft)] cursor-pointer transition-colors"
            >
              {verify.kind === "verifying" || verify.kind === "repairing"
                ? "…"
                : "✓"}
            </span>
            <span
              role="button"
              tabIndex={0}
              title="Uninstall pack"
              onClick={(e) => {
                e.stopPropagation();
                if (!inFlight) runUninstall();
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  e.stopPropagation();
                  if (!inFlight) runUninstall();
                }
              }}
              className="inline-flex items-center px-1.5 py-0.5 rounded text-[10px] uppercase tracking-wide bg-[var(--color-bg-page)]/80 backdrop-blur-sm text-[var(--color-text-bright)]/85 ring-1 ring-[var(--color-bg-raised)]/40 hover:text-[var(--color-status-danger)] cursor-pointer transition-colors"
            >
              {uninstall.kind === "uninstalling" ? "…" : "✕"}
            </span>
          </div>
        </div>
        <div className="p-4">
          <div className="font-medium text-[var(--color-text-bright)] truncate mb-1">
            {record.name}
          </div>
          <div className="text-[10px] text-[var(--color-text-dim)] flex items-center justify-between gap-2">
            <span className="font-mono truncate">
              v{record.version}
              {updateAvailable && (
                <span className="text-[var(--color-accent-soft)]">
                  {" "}
                  → v{latest}
                </span>
              )}
            </span>
            <span className="shrink-0">
              {formatRelativeTime(record.installedAt)}
            </span>
          </div>
        </div>
      </button>
      <VerifyStatus
        state={verify}
        onRepair={runRepair}
        onDismiss={() => setVerify({ kind: "idle" })}
      />
      <UninstallStatus
        state={uninstall}
        onRetry={runUninstall}
        onDismiss={() => setUninstall({ kind: "idle" })}
        onForceRemove={() => onRemove(record)}
      />
    </li>
  );
}

// Inline status block under a history row for uninstall results.
// In the clean-success path the row unmounts (parent drops the
// record from history) so we don't render a state here; only
// partial failures and hard errors land in this component.
function UninstallStatus({
  state,
  onRetry,
  onDismiss,
  onForceRemove,
}: {
  state: RowUninstallState;
  onRetry: () => void;
  onDismiss: () => void;
  onForceRemove: () => void;
}) {
  if (state.kind === "idle") return null;

  if (state.kind === "uninstalling") {
    return (
      <div className="mx-3 mb-2 mt-1 px-2 py-1.5 rounded text-[10px] text-[var(--color-text-dim)] bg-[var(--color-bg-raised)]/30">
        Uninstalling…
      </div>
    );
  }

  if (state.kind === "partial") {
    const stuck = state.report.filesFailed.length;
    return (
      <div className="mx-3 mb-2 mt-1 px-2 py-2 rounded text-[10px] bg-[var(--color-status-warning)]/10 text-[var(--color-status-warning)] space-y-1.5">
        <div>
          Removed {state.report.filesRemoved} files, but {stuck} couldn&apos;t
          be deleted (likely in use by 7DTD).
        </div>
        <div className="text-[var(--color-text-dim)] max-h-16 overflow-y-auto font-mono">
          {state.report.filesFailed.slice(0, 5).map((f) => (
            <div key={f.path} className="truncate">
              {f.path}
            </div>
          ))}
          {stuck > 5 && <div>…and {stuck - 5} more</div>}
        </div>
        <div className="flex gap-1.5">
          <button
            type="button"
            onClick={onRetry}
            className="inline-flex items-center px-2 py-0.5 rounded text-[10px] tracking-wide uppercase bg-[var(--color-accent)]/20 text-[var(--color-accent-soft)] hover:bg-[var(--color-accent)]/30 transition-colors"
          >
            Retry
          </button>
          <button
            type="button"
            onClick={onForceRemove}
            className="inline-flex items-center px-2 py-0.5 rounded text-[10px] tracking-wide uppercase bg-[var(--color-status-danger)]/20 text-[var(--color-status-danger)] hover:bg-[var(--color-status-danger)]/30 transition-colors"
            title="Forget this pack from history without retrying the on-disk delete"
          >
            Drop from list
          </button>
          <button
            type="button"
            onClick={onDismiss}
            className="text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)] text-[10px] px-1"
          >
            dismiss
          </button>
        </div>
      </div>
    );
  }

  // error
  return (
    <div className="mx-3 mb-2 mt-1 px-2 py-1.5 rounded text-[10px] bg-[var(--color-status-danger)]/10 text-[var(--color-status-danger)] space-y-1">
      <div className="break-words">{state.message}</div>
      <button
        type="button"
        onClick={onDismiss}
        className="text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)] text-[10px]"
      >
        dismiss
      </button>
    </div>
  );
}

// Inline status block under a history row. Compact by design — it
// shares the sidebar width so we can't afford a full modal here.
// Three terminal states: healthy (✓), needs-repair (with action),
// repaired (success), plus the obvious in-flight + error states.
function VerifyStatus({
  state,
  onRepair,
  onDismiss,
}: {
  state: RowVerifyState;
  onRepair: () => void;
  onDismiss: () => void;
}) {
  if (state.kind === "idle") return null;

  if (state.kind === "verifying") {
    return (
      <div className="mx-3 mb-2 mt-1 px-2 py-1.5 rounded text-[10px] text-[var(--color-text-dim)] bg-[var(--color-bg-raised)]/30">
        Verifying…
      </div>
    );
  }

  if (state.kind === "repairing") {
    return (
      <div className="mx-3 mb-2 mt-1 px-2 py-1.5 rounded text-[10px] text-[var(--color-text-dim)] bg-[var(--color-bg-raised)]/30">
        Repairing…
      </div>
    );
  }

  if (state.kind === "verified") {
    const failures = state.report.failures.length;
    if (failures === 0) {
      return (
        <div className="mx-3 mb-2 mt-1 px-2 py-1.5 rounded text-[10px] flex items-center justify-between gap-2 bg-[var(--color-status-success)]/10 text-[var(--color-status-success)]">
          <span>✓ {state.report.totalFiles} files OK</span>
          <button
            type="button"
            onClick={onDismiss}
            className="text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)] text-[10px]"
          >
            dismiss
          </button>
        </div>
      );
    }
    const missing = state.report.failures.filter((f) => f.kind === "missing").length;
    const corrupt = failures - missing;
    return (
      <div className="mx-3 mb-2 mt-1 px-2 py-2 rounded text-[10px] bg-[var(--color-status-warning)]/10 text-[var(--color-status-warning)] space-y-1.5">
        <div>
          ⚠ {failures} file{failures === 1 ? "" : "s"} need repair
          {missing > 0 && corrupt > 0 && (
            <span className="text-[var(--color-text-dim)]">
              {" "}
              ({missing} missing, {corrupt} corrupt)
            </span>
          )}
          {missing > 0 && corrupt === 0 && (
            <span className="text-[var(--color-text-dim)]"> ({missing} missing)</span>
          )}
          {corrupt > 0 && missing === 0 && (
            <span className="text-[var(--color-text-dim)]"> ({corrupt} corrupt)</span>
          )}
        </div>
        <div className="flex gap-1.5">
          <button
            type="button"
            onClick={onRepair}
            className="inline-flex items-center px-2 py-0.5 rounded text-[10px] tracking-wide uppercase bg-[var(--color-accent)]/20 text-[var(--color-accent-soft)] hover:bg-[var(--color-accent)]/30 transition-colors"
          >
            Repair
          </button>
          <button
            type="button"
            onClick={onDismiss}
            className="text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)] text-[10px] px-1"
          >
            dismiss
          </button>
        </div>
      </div>
    );
  }

  if (state.kind === "repaired") {
    return (
      <div className="mx-3 mb-2 mt-1 px-2 py-1.5 rounded text-[10px] flex items-center justify-between gap-2 bg-[var(--color-status-success)]/10 text-[var(--color-status-success)]">
        <span>
          ✓ Repaired {state.report.filesRepaired} file
          {state.report.filesRepaired === 1 ? "" : "s"}
        </span>
        <button
          type="button"
          onClick={onDismiss}
          className="text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)] text-[10px]"
        >
          dismiss
        </button>
      </div>
    );
  }

  // error
  return (
    <div className="mx-3 mb-2 mt-1 px-2 py-1.5 rounded text-[10px] bg-[var(--color-status-danger)]/10 text-[var(--color-status-danger)] space-y-1">
      <div className="break-words">{state.message}</div>
      <button
        type="button"
        onClick={onDismiss}
        className="text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)] text-[10px]"
      >
        dismiss
      </button>
    </div>
  );
}

function BrowseView({
  packs,
  error,
  onSelect,
  filters,
  onFiltersChange,
}: {
  packs: CatalogPack[] | null;
  error: string | null;
  onSelect: (p: CatalogPack) => void;
  filters: PackFilters;
  onFiltersChange: (next: PackFilters) => void;
}) {
  if (error) {
    return (
      <div className="px-6 py-12">
        <div className="rounded-lg border border-[var(--color-status-danger)]/40 bg-[var(--color-status-danger)]/10 px-4 py-3 text-sm">
          Couldn&apos;t reach the catalog: {error}
        </div>
      </div>
    );
  }

  if (packs === null) {
    return (
      <div className="px-6 py-12 text-sm text-[var(--color-text-dim)]">
        Loading catalog…
      </div>
    );
  }

  if (packs.length === 0) {
    return (
      <div className="px-6 py-12 text-sm text-[var(--color-text-dim)]">
        No packs available right now.
      </div>
    );
  }

  // Distinct tag list across the catalog. Cheap to compute in
  // every render since the catalog list is bounded at 100 by the
  // server — saves us a separate /tags endpoint.
  const allTags = Array.from(
    new Set(packs.flatMap((p) => p.tags))
  ).sort((a, b) => a.localeCompare(b));

  const qNorm = filters.q.trim().toLowerCase();
  const filtered = packs
    .filter((p) => {
      if (qNorm && !p.name.toLowerCase().includes(qNorm)) return false;
      if (filters.tag && !p.tags.includes(filters.tag)) return false;
      return true;
    })
    .sort((a, b) => {
      if (filters.sort === "new") {
        return (
          new Date(b.createdAt).getTime() - new Date(a.createdAt).getTime()
        );
      }
      if (filters.sort === "favorites") {
        return b.favoriteCount - a.favoriteCount;
      }
      // "popular" — by downloadCount descending. Stable enough as a
      // default since the cloud already serves popular-sorted; this
      // re-sort just preserves order after client-side filtering.
      return b.downloadCount - a.downloadCount;
    });

  const hasFilters =
    !!filters.q.trim() || !!filters.tag || filters.sort !== "popular";
  const clearFilters = () => onFiltersChange(DEFAULT_PACK_FILTERS);

  return (
    <div className="px-6 py-8">
      <div className="mb-5 flex items-end justify-between gap-4 flex-wrap">
        <div>
          <h1 className="text-xl font-semibold tracking-tight">Browse packs</h1>
          <p className="text-xs text-[var(--color-text-dim)] mt-1">
            Click a pack to install it into your 7DTD Mods/ directory.
          </p>
        </div>
        <div className="text-[11px] text-[var(--color-text-dim)] tabular-nums">
          {hasFilters
            ? `${filtered.length} of ${packs.length} packs`
            : `${packs.length} packs`}
        </div>
      </div>

      <div className="mb-5 space-y-3">
        {/* Search + sort row */}
        <div className="flex flex-wrap items-center gap-2">
          <input
            type="search"
            value={filters.q}
            onChange={(e) =>
              onFiltersChange({ ...filters, q: e.currentTarget.value })
            }
            placeholder="Search packs by name…"
            className="flex-1 min-w-[180px] max-w-md rounded-md bg-[var(--color-bg-page)] border border-[var(--color-bg-raised)] px-3 py-1.5 text-sm text-[var(--color-text-bright)] placeholder:text-[var(--color-text-dim)]/60 outline-none focus:border-[var(--color-accent-soft)]/60 focus:ring-2 focus:ring-[var(--color-accent)]/20 transition-colors"
          />
          <span className="text-[11px] text-[var(--color-text-dim)] ml-2">
            Sort
          </span>
          <SortChip
            active={filters.sort === "popular"}
            onClick={() => onFiltersChange({ ...filters, sort: "popular" })}
          >
            Popular
          </SortChip>
          <SortChip
            active={filters.sort === "new"}
            onClick={() => onFiltersChange({ ...filters, sort: "new" })}
          >
            Newest
          </SortChip>
          <SortChip
            active={filters.sort === "favorites"}
            onClick={() => onFiltersChange({ ...filters, sort: "favorites" })}
          >
            Favorites
          </SortChip>
          {hasFilters && (
            <button
              type="button"
              onClick={clearFilters}
              className="ml-auto text-[10px] tracking-[0.14em] uppercase text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)] transition-colors"
            >
              Clear filters
            </button>
          )}
        </div>

        {/* Tag chips — only render when the catalog actually has
            tags so we don't show an empty row. "All" pill clears
            the tag filter without changing q or sort. */}
        {allTags.length > 0 && (
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-[11px] text-[var(--color-text-dim)] mr-1">
              Tag
            </span>
            <SortChip
              active={!filters.tag}
              onClick={() => onFiltersChange({ ...filters, tag: "" })}
            >
              All
            </SortChip>
            {allTags.map((t) => (
              <SortChip
                key={t}
                active={filters.tag === t}
                onClick={() =>
                  onFiltersChange({
                    ...filters,
                    tag: filters.tag === t ? "" : t,
                  })
                }
              >
                {t}
              </SortChip>
            ))}
          </div>
        )}
      </div>

      {filtered.length === 0 ? (
        <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 px-6 py-10 text-center">
          <div className="text-sm text-[var(--color-text-bright)] font-medium mb-1">
            No packs match these filters
          </div>
          <p className="text-xs text-[var(--color-text-dim)] mb-4">
            Try a different tag, drop the search, or clear all filters.
          </p>
          <button
            type="button"
            onClick={clearFilters}
            className="inline-flex items-center px-3.5 py-1.5 rounded-md border border-[var(--color-bg-raised)] hover:border-[var(--color-accent-soft)]/40 hover:text-[var(--color-text-bright)] text-[var(--color-text-bright)]/85 text-xs"
          >
            Clear filters
          </button>
        </div>
      ) : (
        <ul className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4">
          {filtered.map((p) => (
            <li key={p.slug}>
              <button
                type="button"
                onClick={() => onSelect(p)}
                className="w-full text-left rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)] hover:border-[var(--color-accent-soft)]/40 transition-colors overflow-hidden group"
              >
                <div className="aspect-[16/9] bg-[var(--color-bg-raised)] relative overflow-hidden">
                  <div className="absolute inset-0 flex items-center justify-center text-[var(--color-text-dim)]/50 text-xs tracking-[0.2em] uppercase">
                    no cover
                  </div>
                  <CoverImage
                    src={p.coverImage}
                    className="absolute inset-0 w-full h-full object-cover group-hover:scale-105 transition-transform duration-500"
                  />
                  <div className="absolute inset-0 bg-gradient-to-t from-[var(--color-bg-panel)] via-transparent to-transparent" />
                  {p.isFeatured && (
                    // Mirrors the cloud /browse "Featured" chip —
                    // gold star + label, top-left of the cover.
                    // Admin curation surface; publishers can't
                    // self-pin.
                    <span
                      className="absolute top-2 left-2 inline-flex items-center gap-1 text-[9px] tracking-[0.18em] uppercase font-semibold text-[var(--color-bg-page)] bg-[var(--color-accent-soft)] px-1.5 py-0.5 rounded shadow-sm"
                      title="Editor's pick"
                    >
                      <FeaturedStarGlyph />
                      Featured
                    </span>
                  )}
                </div>
                <div className="p-4">
                  <div className="flex items-baseline gap-2 mb-1">
                    <span className="font-medium text-[var(--color-text-bright)] truncate">
                      {p.name}
                    </span>
                    {p.latestVersion && (
                      <span className="font-mono text-[10px] text-[var(--color-text-dim)]">
                        v{p.latestVersion}
                      </span>
                    )}
                  </div>
                  <div className="text-[10px] text-[var(--color-text-dim)] mb-2 truncate">
                    by {p.publisherName}
                  </div>
                  <p className="text-xs text-[var(--color-text-dim)] leading-relaxed line-clamp-2">
                    {p.summary ?? ""}
                  </p>
                  {p.tags.length > 0 && (
                    <div className="mt-2 flex flex-wrap gap-1">
                      {p.tags.slice(0, 3).map((t) => (
                        <span
                          key={t}
                          className="text-[9px] tracking-wide uppercase px-1.5 py-0.5 rounded bg-[var(--color-bg-raised)]/60 text-[var(--color-text-bright)]/75"
                        >
                          {t}
                        </span>
                      ))}
                    </div>
                  )}
                  <div className="mt-3 flex items-center justify-between text-[10px] text-[var(--color-text-dim)]">
                    <span className="inline-flex items-center gap-1.5">
                      <HeartGlyph filled={false} />
                      <span className="tabular-nums">
                        {p.favoriteCount.toLocaleString()}
                      </span>
                      <span className="text-[var(--color-bg-raised)]">·</span>
                      {/* Install count — derived from the catalog's
                        * downloadCount column (signed-manifest fetches
                        * count once per profile install). Sits between
                        * favorites and files so it's visible at a
                        * glance but doesn't dominate the row. */}
                      <DownloadGlyph />
                      <span
                        className="tabular-nums"
                        title={`${p.downloadCount.toLocaleString()} installs`}
                      >
                        {p.downloadCount.toLocaleString()}
                      </span>
                      <span className="text-[var(--color-bg-raised)]">·</span>
                      <span>{p.fileCount.toLocaleString()} files</span>
                    </span>
                    <span>{formatBytes(p.totalSizeBytes)}</span>
                  </div>
                </div>
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

// Pill-shaped sort/tag chip — matches the website's chipClass.
// Reused for both sort buttons and tag chips since they share the
// "two-state pill" pattern.
function SortChip({
  active,
  onClick,
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`inline-flex items-center px-3 py-1 rounded-full border text-[11px] tracking-wide uppercase transition-colors ${
        active
          ? "border-[var(--color-accent-soft)]/50 bg-[var(--color-accent)]/15 text-[var(--color-accent-soft)]"
          : "border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 text-[var(--color-text-dim)] hover:border-[var(--color-accent-soft)]/30 hover:text-[var(--color-text-bright)]"
      }`}
    >
      {children}
    </button>
  );
}

function InstallView({
  pack,
  defaultDest,
  installedRecord,
  connectAddress,
  backLabel,
  onBack,
  onInstalled,
  onUpdated,
}: {
  pack: CatalogPack;
  defaultDest: string;
  /** History entry for this pack, if it's already installed.
   *  Drives the InstallView's mode (install vs update vs reinstall)
   *  and locks dest to the on-disk path so update's manifest diff
   *  applies to the right directory. */
  installedRecord: InstallRecord | null;
  /** When the install was launched from a server detail page, the
   *  server's connect address gets surfaced under the "done" state
   *  with a copy button — the actual point of "one-click join." */
  connectAddress: string | null;
  /** What the back button reads — "BROWSE" for pack browse,
   *  the server's name when launched from a server detail page. */
  backLabel: string;
  onBack: () => void;
  onInstalled: (report: InstallReport) => void;
  onUpdated: (report: UpdateReport) => void;
}) {
  // Mode is derived from history + catalog state. Update fires when
  // both the installed record AND a newer catalog version exist;
  // reinstall covers the "same version on disk" or "older catalog"
  // edge cases (which can happen if a publisher pulls a version).
  const mode: InstallMode = (() => {
    if (!installedRecord) return "install";
    if (
      pack.latestVersion &&
      semverIsNewer(pack.latestVersion, installedRecord.version)
    ) {
      return "update";
    }
    return "reinstall";
  })();
  const destLocked = mode === "update";

  const [dest, setDest] = useState(defaultDest);
  const [state, setState] = useState<InstallState>({ kind: "idle" });

  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    (async () => {
      unlisten = await listen<InstallProgress>("install://progress", (e) => {
        setState((prev) => {
          if (prev.kind !== "running") return prev;
          return { kind: "running", progress: e.payload };
        });
      });
    })();
    return () => {
      if (unlisten) unlisten();
    };
  }, []);

  async function pickFolder() {
    try {
      const picked = await openDialog({
        directory: true,
        multiple: false,
        title: "Pick your 7DTD Mods/ directory",
      });
      if (typeof picked === "string" && picked) {
        setDest(picked);
      }
    } catch {
      // User cancelled, or dialog isn't available — leave dest as-is.
    }
  }

  async function startInstall() {
    setState({ kind: "running", progress: null });
    try {
      if (mode === "update") {
        const report = await invoke<UpdateReport>("update_pack", {
          slug: pack.slug,
          dest,
        });
        setState({ kind: "done", result: { mode: "update", report } });
        onUpdated(report);
      } else {
        const report = await invoke<InstallReport>("install_pack", {
          slug: pack.slug,
          dest,
        });
        setState({ kind: "done", result: { mode, report } });
        onInstalled(report);
      }
    } catch (e) {
      setState({
        kind: "error",
        message: typeof e === "string" ? e : `${e}`,
      });
    }
  }

  const primaryButtonLabel =
    mode === "update"
      ? `Update to v${pack.latestVersion ?? "?"}`
      : mode === "reinstall"
        ? "Re-install"
        : "Install pack";
  const errorTitle =
    mode === "update" ? "Update failed" : "Install failed";
  const inFlightLabel = mode === "update" ? "Updating…" : "Installing…";

  const running = state.kind === "running";
  const progress = state.kind === "running" ? state.progress : null;
  const pct =
    progress && progress.totalBytes > 0
      ? Math.min(100, (progress.bytesSoFar / progress.totalBytes) * 100)
      : 0;

  return (
    <div className="px-6 py-8 max-w-2xl mx-auto">
      <button
        type="button"
        onClick={onBack}
        disabled={running}
        className="text-[10px] tracking-[0.18em] uppercase text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)] mb-4 disabled:opacity-40 disabled:cursor-not-allowed"
      >
        ← {backLabel}
      </button>

      <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)] overflow-hidden mb-6">
        <div className="aspect-[16/9] bg-[var(--color-bg-raised)] relative">
          <div className="absolute inset-0 flex items-center justify-center text-[var(--color-text-dim)]/40 text-xs tracking-[0.2em] uppercase">
            no cover
          </div>
          <CoverImage
            src={pack.coverImage}
            className="absolute inset-0 w-full h-full object-cover"
          />
          <div className="absolute inset-0 bg-gradient-to-t from-[var(--color-bg-panel)] via-[var(--color-bg-panel)]/40 to-transparent" />
          <div className="absolute bottom-4 left-5 right-5">
            <div className="text-xl font-semibold drop-shadow-lg">
              {pack.name}
            </div>
            <div className="text-xs text-[var(--color-text-dim)] mt-1">
              by {pack.publisherName}
              {pack.latestVersion && (
                <span className="font-mono"> · v{pack.latestVersion}</span>
              )}
              {" · "}
              {pack.fileCount.toLocaleString()} files ·{" "}
              {formatBytes(pack.totalSizeBytes)}
            </div>
          </div>
        </div>
        {pack.summary && (
          <p className="px-5 py-4 text-sm text-[var(--color-text-bright)]/85 leading-relaxed border-t border-[var(--color-bg-raised)]">
            {pack.summary}
          </p>
        )}
      </div>

      <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 p-5">
        <label
          htmlFor="dest"
          className="block text-[10px] font-medium tracking-[0.14em] uppercase text-[var(--color-text-bright)]/85 mb-2"
        >
          Install to
        </label>
        <div className="flex gap-2">
          <input
            id="dest"
            type="text"
            value={dest}
            onChange={(e) => setDest(e.currentTarget.value)}
            disabled={running || destLocked}
            className="flex-1 min-w-0 rounded-md bg-[var(--color-bg-page)] border border-[var(--color-bg-raised)] px-3 py-2 text-sm font-mono text-[var(--color-text-bright)] outline-none focus:border-[var(--color-accent-soft)]/60 focus:ring-2 focus:ring-[var(--color-accent)]/20 transition-colors disabled:opacity-60"
            placeholder="Path to your 7DTD Mods/ directory"
            spellCheck={false}
          />
          <button
            type="button"
            onClick={pickFolder}
            disabled={running || destLocked}
            className="shrink-0 inline-flex items-center gap-1.5 px-3 py-2 rounded-md border border-[var(--color-bg-raised)] hover:border-[var(--color-accent-soft)]/40 hover:text-[var(--color-text-bright)] text-[var(--color-text-bright)]/85 text-xs font-medium transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
            title="Open a folder picker"
          >
            <FolderGlyph />
            Browse…
          </button>
        </div>
        <p className="mt-1.5 text-[11px] text-[var(--color-text-dim)] leading-relaxed">
          {destLocked && installedRecord ? (
            <>
              Update applies to the existing install at this path —
              only changed files are refetched. To install fresh
              elsewhere, uninstall first.
            </>
          ) : (
            <>
              The pack folder lands inside this directory. On Windows the
              standard path is{" "}
              <code className="font-mono">%APPDATA%\7DaysToDie\Mods</code>.
            </>
          )}
        </p>
        {mode === "update" && installedRecord && (
          <div className="mt-3 rounded-md border border-[var(--color-accent-soft)]/40 bg-[var(--color-accent)]/10 px-3 py-2 text-[11px] flex items-center justify-between gap-3">
            <span>
              <span className="font-mono text-[var(--color-accent-soft)]">
                v{installedRecord.version}
              </span>
              <span className="text-[var(--color-text-dim)]"> → </span>
              <span className="font-mono text-[var(--color-accent-soft)]">
                v{pack.latestVersion}
              </span>
            </span>
            <span className="text-[var(--color-text-dim)]">
              Smart update: only changed files are refetched.
            </span>
          </div>
        )}
        {mode === "reinstall" && installedRecord && (
          <div className="mt-3 rounded-md border border-[var(--color-bg-raised)] bg-[var(--color-bg-page)]/40 px-3 py-2 text-[11px] text-[var(--color-text-dim)]">
            v{installedRecord.version} already installed here.
            Re-installing fetches the full pack again.
          </div>
        )}

        <div className="mt-5">
          {state.kind === "idle" && (
            <button
              type="button"
              onClick={startInstall}
              disabled={!dest.trim()}
              className="inline-flex items-center gap-2 px-5 py-2.5 rounded-md bg-[var(--color-accent)] hover:bg-[var(--color-accent)]/90 disabled:opacity-50 disabled:cursor-not-allowed transition-colors text-sm font-medium"
            >
              {primaryButtonLabel}
            </button>
          )}
          {state.kind === "running" && (
            <div>
              <div className="flex items-center justify-between text-xs mb-1.5">
                <span className="text-[var(--color-text-dim)]">
                  {inFlightLabel}
                </span>
                <span className="font-mono text-[var(--color-text-bright)]/85">
                  {progress
                    ? `${formatBytes(progress.bytesSoFar)} / ${formatBytes(progress.totalBytes)}`
                    : "—"}
                </span>
              </div>
              <div className="h-2 rounded-full bg-[var(--color-bg-raised)] overflow-hidden">
                <div
                  className="h-full bg-[var(--color-accent)] transition-[width] duration-200"
                  style={{ width: `${Math.max(2, pct)}%` }}
                />
              </div>
              {progress?.lastCompletedFile && (
                <div className="mt-2 text-[10px] font-mono text-[var(--color-text-dim)] truncate">
                  ✓ {progress.lastCompletedFile}
                </div>
              )}
            </div>
          )}
          {state.kind === "done" && state.result.mode === "update" && (
            <div className="space-y-3">
              <div className="rounded-md border border-[var(--color-status-success)]/40 bg-[var(--color-status-success)]/10 px-4 py-3 text-sm">
                <div className="font-medium mb-1">
                  ✓ {state.result.report.displayName} updated to v
                  {state.result.report.toVersion}
                </div>
                <div className="text-xs text-[var(--color-text-dim)]">
                  {state.result.report.filesChanged} changed,{" "}
                  {state.result.report.filesAdded} new,{" "}
                  {state.result.report.filesRemoved} removed,{" "}
                  {state.result.report.filesKept} kept · downloaded{" "}
                  {formatBytes(state.result.report.bytesDownloaded)}
                </div>
              </div>
              {connectAddress && (
                <div className="rounded-md border border-[var(--color-accent-soft)]/40 bg-[var(--color-accent)]/10 px-4 py-3">
                  <div className="text-[10px] tracking-[0.14em] uppercase text-[var(--color-accent-soft)] mb-2">
                    Now connect in 7DTD
                  </div>
                  <LaunchPanel address={connectAddress} />
                </div>
              )}
            </div>
          )}
          {state.kind === "done" && state.result.mode !== "update" && (
            <div className="space-y-3">
              <div className="rounded-md border border-[var(--color-status-success)]/40 bg-[var(--color-status-success)]/10 px-4 py-3 text-sm">
                <div className="font-medium mb-1">
                  ✓ {state.result.report.displayName} v
                  {state.result.report.version}{" "}
                  {state.result.mode === "reinstall" ? "re-installed" : "installed"}
                </div>
                <div className="text-xs text-[var(--color-text-dim)]">
                  {state.result.report.fileCount} files,{" "}
                  {formatBytes(state.result.report.totalBytes)} →{" "}
                  <code className="font-mono">{state.result.report.dest}</code>
                </div>
              </div>
              {connectAddress && (
                <div className="rounded-md border border-[var(--color-accent-soft)]/40 bg-[var(--color-accent)]/10 px-4 py-3">
                  <div className="text-[10px] tracking-[0.14em] uppercase text-[var(--color-accent-soft)] mb-2">
                    Now connect in 7DTD
                  </div>
                  <LaunchPanel address={connectAddress} />
                </div>
              )}
            </div>
          )}
          {state.kind === "error" && (
            <div className="rounded-md border border-[var(--color-status-danger)]/40 bg-[var(--color-status-danger)]/10 px-4 py-3 text-sm">
              <div className="font-medium mb-1">{errorTitle}</div>
              <div className="text-xs font-mono whitespace-pre-wrap break-words">
                {state.message}
              </div>
              <button
                type="button"
                onClick={startInstall}
                className="mt-3 inline-flex items-center gap-2 px-4 py-1.5 rounded-md border border-[var(--color-bg-raised)] hover:border-[var(--color-accent-soft)]/40 hover:text-[var(--color-text-bright)] text-[var(--color-text-bright)]/85 text-xs"
              >
                Retry
              </button>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

// Rich pack page — what Browse cards land on. Deliberately slower-
// paced than InstallView: scrollable description, full tag list,
// stats, and the install button as a single deliberate CTA at the
// bottom (mirroring the eventual InstallView mode label so the
// user isn't surprised on the next screen).
// Mirrors the Rust PackDirEntry — one top-level mod inside a pack
// manifest. DetailView fetches these lazily so the page renders
// before the network roundtrip lands; missing data just hides the
// "What's inside" section.
type PackDirEntry = {
  name: string;
  fileCount: number;
  totalBytes: number;
};

function DetailView({
  pack,
  installedRecord,
  favorited,
  signedIn,
  onToggleFavorite,
  onSignInRequest,
  onBack,
  onInstall,
}: {
  pack: CatalogPack;
  installedRecord: InstallRecord | null;
  favorited: boolean;
  signedIn: boolean;
  onToggleFavorite: () => void;
  onSignInRequest: () => void;
  onBack: () => void;
  onInstall: () => void;
}) {
  // "What's inside" list — the top-level dirs in the manifest,
  // which by 7DTD's pack-on-disk convention are the names of the
  // mods bundled inside the pack. Fetched lazily so the page
  // first-paint is instant; if the fetch fails we just don't
  // show the section.
  const [insideDirs, setInsideDirs] = useState<PackDirEntry[] | null>(null);
  useEffect(() => {
    let cancelled = false;
    setInsideDirs(null);
    (async () => {
      try {
        const dirs = await invoke<PackDirEntry[]>("fetch_pack_overview", {
          slug: pack.slug,
        });
        if (!cancelled) setInsideDirs(dirs);
      } catch {
        // Manifest network error or pack-not-found — leave the
        // section hidden. Not worth a banner; the stats panel
        // above still tells the user the pack exists.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [pack.slug]);

  // Mirrors the mode-detection in InstallView so the action button
  // here reads the same as what'll show on the next screen.
  const mode: InstallMode = (() => {
    if (!installedRecord) return "install";
    if (
      pack.latestVersion &&
      semverIsNewer(pack.latestVersion, installedRecord.version)
    ) {
      return "update";
    }
    return "reinstall";
  })();
  const actionLabel =
    mode === "update"
      ? `Update to v${pack.latestVersion ?? "?"}`
      : mode === "reinstall"
        ? "Re-install"
        : "Install pack";

  const ageDays = (() => {
    const dt = new Date(pack.createdAt).getTime();
    return Math.max(0, Math.floor((Date.now() - dt) / 86_400_000));
  })();
  const ageLabel =
    ageDays < 1
      ? "Today"
      : ageDays < 30
        ? `${ageDays}d ago`
        : ageDays < 365
          ? `${Math.floor(ageDays / 30)}mo ago`
          : `${Math.floor(ageDays / 365)}y ago`;

  return (
    <div className="px-6 py-8 max-w-3xl mx-auto">
      <div className="flex items-center justify-between gap-3 mb-4">
        <button
          type="button"
          onClick={onBack}
          className="text-[10px] tracking-[0.18em] uppercase text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)]"
        >
          ← BROWSE
        </button>
        <HeartButton
          count={pack.favoriteCount}
          favorited={favorited}
          signedIn={signedIn}
          onToggle={onToggleFavorite}
          onSignInRequest={onSignInRequest}
        />
      </div>

      <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)] overflow-hidden mb-6">
        <div className="aspect-[16/9] bg-[var(--color-bg-raised)] relative">
          <div className="absolute inset-0 flex items-center justify-center text-[var(--color-text-dim)]/40 text-xs tracking-[0.2em] uppercase">
            no cover
          </div>
          <CoverImage
            src={pack.coverImage}
            className="absolute inset-0 w-full h-full object-cover"
          />
          <div className="absolute inset-0 bg-gradient-to-t from-[var(--color-bg-panel)] via-[var(--color-bg-panel)]/30 to-transparent" />
          <div className="absolute bottom-5 left-6 right-6">
            <div className="text-2xl font-semibold drop-shadow-lg">
              {pack.name}
            </div>
            <div className="text-xs text-[var(--color-text-bright)]/85 mt-1">
              by {pack.publisherName}
              {pack.latestVersion && (
                <>
                  <span className="text-[var(--color-text-dim)]"> · </span>
                  <span className="font-mono">v{pack.latestVersion}</span>
                </>
              )}
              {installedRecord && (
                <span className="ml-2 inline-flex items-center px-1.5 py-0.5 rounded text-[9px] tracking-wide uppercase font-medium bg-[var(--color-status-success)]/15 text-[var(--color-status-success)] ring-1 ring-[var(--color-status-success)]/40 align-middle">
                  installed v{installedRecord.version}
                </span>
              )}
            </div>
          </div>
        </div>
      </div>

      {pack.summary && (
        <p className="text-base text-[var(--color-text-bright)]/90 leading-relaxed mb-5">
          {pack.summary}
        </p>
      )}

      {pack.tags.length > 0 && (
        <div className="flex flex-wrap gap-1.5 mb-6">
          {pack.tags.map((t) => (
            <span
              key={t}
              className="inline-flex items-center px-2 py-0.5 rounded-full text-[10px] tracking-wide uppercase font-medium bg-[var(--color-bg-raised)]/60 text-[var(--color-text-bright)]/85 border border-[var(--color-bg-raised)]"
            >
              {t}
            </span>
          ))}
        </div>
      )}

      <div className="grid grid-cols-2 sm:grid-cols-4 gap-3 mb-6">
        <StatCard label="Files" value={pack.fileCount.toLocaleString()} />
        <StatCard label="Size" value={formatBytes(pack.totalSizeBytes)} />
        <StatCard
          label="Downloads"
          value={pack.downloadCount.toLocaleString()}
        />
        <StatCard label="Published" value={ageLabel} />
      </div>

      {insideDirs && insideDirs.length > 0 && (
        <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/40 px-5 py-5 mb-6">
          <h2 className="text-[10px] font-medium tracking-[0.14em] uppercase text-[var(--color-text-bright)]/85 mb-3 flex items-baseline justify-between gap-2">
            <span>What&apos;s inside</span>
            <span className="text-[10px] text-[var(--color-text-dim)] font-normal tracking-normal normal-case">
              {insideDirs.length} mod{insideDirs.length === 1 ? "" : "s"}
            </span>
          </h2>
          <ul className="grid grid-cols-1 sm:grid-cols-2 gap-1.5">
            {insideDirs.map((d) => (
              <li
                key={d.name}
                className="flex items-center justify-between gap-3 rounded-md border border-[var(--color-bg-raised)] bg-[var(--color-bg-page)]/40 px-3 py-2"
              >
                <span
                  className={`text-[12px] truncate ${
                    d.name === "(root)"
                      ? "text-[var(--color-text-dim)] italic"
                      : "text-[var(--color-text-bright)] font-medium"
                  }`}
                  title={d.name}
                >
                  {d.name}
                </span>
                <span className="shrink-0 text-[10px] text-[var(--color-text-dim)] tabular-nums">
                  {d.fileCount}f · {formatBytes(d.totalBytes)}
                </span>
              </li>
            ))}
          </ul>
        </div>
      )}

      {pack.description && (
        <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/40 px-5 py-5 mb-6">
          <h2 className="text-[10px] font-medium tracking-[0.14em] uppercase text-[var(--color-text-bright)]/85 mb-3">
            About this pack
          </h2>
          {/* Render description as preformatted text so newlines and
              paragraph breaks survive. Future iteration: parse as
              Markdown if publishers want links/headings/lists. */}
          <p className="text-sm text-[var(--color-text-bright)]/85 leading-relaxed whitespace-pre-wrap">
            {pack.description}
          </p>
        </div>
      )}

      <div className="sticky bottom-4 mt-8 rounded-xl border border-[var(--color-accent-soft)]/30 bg-[var(--color-bg-panel)]/90 backdrop-blur-sm px-5 py-4 flex items-center justify-between gap-4 shadow-lg">
        <div className="min-w-0 flex-1">
          <div className="text-sm font-medium text-[var(--color-text-bright)] truncate">
            {pack.name}
            {pack.latestVersion && (
              <span className="font-mono text-[11px] text-[var(--color-text-dim)] ml-1.5">
                v{pack.latestVersion}
              </span>
            )}
          </div>
          <div className="text-[11px] text-[var(--color-text-dim)] mt-0.5">
            {pack.fileCount.toLocaleString()} files ·{" "}
            {formatBytes(pack.totalSizeBytes)}
          </div>
        </div>
        <button
          type="button"
          onClick={onInstall}
          className="shrink-0 inline-flex items-center gap-2 px-5 py-2.5 rounded-md bg-[var(--color-accent)] hover:bg-[var(--color-accent)]/90 transition-colors text-sm font-medium"
        >
          {actionLabel}
        </button>
      </div>
    </div>
  );
}

function StatCard({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-md border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/40 px-3 py-2.5">
      <div className="text-[9px] font-medium tracking-[0.14em] uppercase text-[var(--color-text-dim)] mb-1">
        {label}
      </div>
      <div className="text-sm font-medium text-[var(--color-text-bright)] tabular-nums">
        {value}
      </div>
    </div>
  );
}

function ServerBrowseView({
  servers,
  error,
  onSelect,
  filters,
  onFiltersChange,
}: {
  servers: CatalogServer[] | null;
  error: string | null;
  onSelect: (s: CatalogServer) => void;
  filters: ServerFilters;
  onFiltersChange: (next: ServerFilters) => void;
}) {
  if (error) {
    return (
      <div className="px-6 py-12">
        <div className="rounded-lg border border-[var(--color-status-danger)]/40 bg-[var(--color-status-danger)]/10 px-4 py-3 text-sm">
          Couldn&apos;t reach the server catalog: {error}
        </div>
      </div>
    );
  }
  if (servers === null) {
    return (
      <div className="px-6 py-12 text-sm text-[var(--color-text-dim)]">
        Loading servers…
      </div>
    );
  }
  if (servers.length === 0) {
    return (
      <div className="px-6 py-12 text-sm text-[var(--color-text-dim)]">
        No active servers right now.
      </div>
    );
  }

  return (
    <ServerBrowseLoaded
      servers={servers}
      onSelect={onSelect}
      filters={filters}
      onFiltersChange={onFiltersChange}
    />
  );
}

// All the regions the catalog supports; "" means "any region" in
// the filter state. Order matches the website's /servers page so
// users moving between launcher and web see the same list.
const FILTER_REGIONS: { value: string; label: string }[] = [
  { value: "", label: "All regions" },
  { value: "na_east", label: "NA East" },
  { value: "na_west", label: "NA West" },
  { value: "eu", label: "Europe" },
  { value: "as", label: "Asia" },
  { value: "oc", label: "Oceania" },
  { value: "sa", label: "South America" },
  { value: "af", label: "Africa" },
];

function ServerBrowseLoaded({
  servers,
  onSelect,
  filters,
  onFiltersChange,
}: {
  servers: CatalogServer[];
  onSelect: (s: CatalogServer) => void;
  filters: ServerFilters;
  onFiltersChange: (next: ServerFilters) => void;
}) {
  const { region, onlineOnly, notFull, sort } = filters;
  const setRegion = (v: string) => onFiltersChange({ ...filters, region: v });
  const setOnlineOnly = (v: boolean) =>
    onFiltersChange({ ...filters, onlineOnly: v });
  const setNotFull = (v: boolean) =>
    onFiltersChange({ ...filters, notFull: v });
  const setSort = (v: ServerSort) =>
    onFiltersChange({ ...filters, sort: v });

  const filtered = servers
    .filter((s) => {
      if (region && s.region !== region) return false;
      if (onlineOnly && !s.online) return false;
      if (notFull && s.currentPlayers >= s.maxPlayers) return false;
      return true;
    })
    .slice()
    .sort((a, b) => {
      // Three sort modes, all desc. Stable-ish on ties via slug
      // tiebreaker so reorders during a poll don't shuffle the
      // list visibly.
      if (sort === "new") {
        const ax = new Date(a.createdAt).getTime();
        const bx = new Date(b.createdAt).getTime();
        if (bx !== ax) return bx - ax;
      } else if (sort === "favorites") {
        if (b.favoriteCount !== a.favoriteCount) {
          return b.favoriteCount - a.favoriteCount;
        }
      } else {
        // "players" — busiest server first. Online servers always
        // outrank offline ones in this mode (an offline server
        // claiming "100 players" wouldn't be more interesting than
        // a live one with 4).
        if (a.online !== b.online) return a.online ? -1 : 1;
        if (b.currentPlayers !== a.currentPlayers) {
          return b.currentPlayers - a.currentPlayers;
        }
      }
      return a.slug.localeCompare(b.slug);
    });
  const hasFilters =
    !!region || onlineOnly || notFull || sort !== DEFAULT_SERVER_FILTERS.sort;
  const clearFilters = () => onFiltersChange(DEFAULT_SERVER_FILTERS);

  return (
    <div className="px-6 py-8">
      <div className="mb-5 flex items-end justify-between gap-4 flex-wrap">
        <div>
          <h1 className="text-xl font-semibold tracking-tight">Browse servers</h1>
          <p className="text-xs text-[var(--color-text-dim)] mt-1">
            Click a server to install its attached pack and grab the
            connect address.
          </p>
        </div>
        <div className="text-[11px] text-[var(--color-text-dim)] tabular-nums">
          {hasFilters
            ? `${filtered.length} of ${servers.length} servers`
            : `${servers.length} servers`}
        </div>
      </div>

      <div className="mb-5 flex flex-wrap items-center gap-2">
        <label className="text-[11px] text-[var(--color-text-dim)] mr-1">
          Region
        </label>
        <select
          value={region}
          onChange={(e) => setRegion(e.currentTarget.value)}
          className="rounded-md bg-[var(--color-bg-page)] border border-[var(--color-bg-raised)] px-2.5 py-1.5 text-xs text-[var(--color-text-bright)] outline-none focus:border-[var(--color-accent-soft)]/60 focus:ring-2 focus:ring-[var(--color-accent)]/20 transition-colors hover:border-[var(--color-accent-soft)]/40"
        >
          {FILTER_REGIONS.map((r) => (
            <option key={r.value} value={r.value}>
              {r.label}
            </option>
          ))}
        </select>
        <span className="mx-1 text-[var(--color-bg-raised)]">·</span>
        <FilterChip
          active={onlineOnly}
          onClick={() => setOnlineOnly(!onlineOnly)}
          dotColor="success"
        >
          Online only
        </FilterChip>
        <FilterChip
          active={notFull}
          onClick={() => setNotFull(!notFull)}
          dotColor="accent"
        >
          Not full
        </FilterChip>
        <span className="mx-1 text-[var(--color-bg-raised)]">·</span>
        <span className="text-[11px] text-[var(--color-text-dim)] mr-1">
          Sort
        </span>
        <SortChip
          active={sort === "players"}
          onClick={() => setSort("players")}
        >
          Players
        </SortChip>
        <SortChip active={sort === "new"} onClick={() => setSort("new")}>
          Newest
        </SortChip>
        <SortChip
          active={sort === "favorites"}
          onClick={() => setSort("favorites")}
        >
          Favorites
        </SortChip>
        {hasFilters && (
          <button
            type="button"
            onClick={clearFilters}
            className="ml-auto text-[10px] tracking-[0.14em] uppercase text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)] transition-colors"
          >
            Clear filters
          </button>
        )}
      </div>

      {filtered.length === 0 ? (
        <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 px-6 py-10 text-center">
          <div className="text-sm text-[var(--color-text-bright)] font-medium mb-1">
            No servers match these filters
          </div>
          <p className="text-xs text-[var(--color-text-dim)] mb-4">
            Try a different region, or drop a filter.
          </p>
          <button
            type="button"
            onClick={clearFilters}
            className="inline-flex items-center px-3.5 py-1.5 rounded-md border border-[var(--color-bg-raised)] hover:border-[var(--color-accent-soft)]/40 hover:text-[var(--color-text-bright)] text-[var(--color-text-bright)]/85 text-xs"
          >
            Clear filters
          </button>
        </div>
      ) : (
        <ul className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4">
          {filtered.map((s) => (
            <li key={s.slug}>
              <ServerCard server={s} onSelect={onSelect} />
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

// Toggle chip used in the server filter bar. Two visual states:
// active = accent-tinted, inactive = subdued border. The colored
// dot signals which kind of filter it is (success for liveness,
// accent for availability).
function FilterChip({
  active,
  onClick,
  dotColor,
  children,
}: {
  active: boolean;
  onClick: () => void;
  dotColor: "success" | "accent";
  children: React.ReactNode;
}) {
  const dotClass =
    dotColor === "success"
      ? active
        ? "bg-[var(--color-status-success)]"
        : "bg-[var(--color-text-dim)]/60"
      : active
        ? "bg-[var(--color-accent-soft)]"
        : "bg-[var(--color-text-dim)]/60";
  return (
    <button
      type="button"
      onClick={onClick}
      className={`inline-flex items-center gap-1.5 px-3 py-1 rounded-full border text-[11px] tracking-wide uppercase transition-colors ${
        active
          ? "border-[var(--color-accent-soft)]/50 bg-[var(--color-accent)]/15 text-[var(--color-accent-soft)]"
          : "border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 text-[var(--color-text-dim)] hover:border-[var(--color-accent-soft)]/30 hover:text-[var(--color-text-bright)]"
      }`}
    >
      <span className={`size-1.5 rounded-full ${dotClass}`} />
      {children}
    </button>
  );
}

// Uptime-tier color: ≥90 green, ≥50 neutral-bright, else warning.
// Matches the website's /servers card so the same server reads the
// same in both places.
function uptimeTier(pct: number): {
  bg: string;
  text: string;
  ring: string;
} {
  if (pct >= 90) {
    return {
      bg: "bg-[var(--color-status-success)]/10",
      text: "text-[var(--color-status-success)]",
      ring: "ring-[var(--color-status-success)]/40",
    };
  }
  if (pct >= 50) {
    return {
      bg: "bg-[var(--color-bg-raised)]/40",
      text: "text-[var(--color-text-bright)]/85",
      ring: "ring-[var(--color-bg-raised)]",
    };
  }
  return {
    bg: "bg-[var(--color-status-warning)]/10",
    text: "text-[var(--color-status-warning)]",
    ring: "ring-[var(--color-status-warning)]/40",
  };
}

// One card in the server grid. Extracted so the filter logic in
// ServerBrowseLoaded stays readable, and so we can color-code the
// players + uptime indicators in one place.
function ServerCard({
  server: s,
  onSelect,
}: {
  server: CatalogServer;
  onSelect: (s: CatalogServer) => void;
}) {
  const ut = uptimeTier(s.uptimePct);
  const isFull = s.online && s.currentPlayers >= s.maxPlayers;
  return (
    <button
      type="button"
      onClick={() => onSelect(s)}
      className="w-full text-left rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)] hover:border-[var(--color-accent-soft)]/40 transition-colors overflow-hidden group"
    >
      <div className="aspect-[16/9] bg-[var(--color-bg-raised)] relative overflow-hidden">
        <div className="absolute inset-0 flex items-center justify-center text-[var(--color-text-dim)]/50 text-xs tracking-[0.2em] uppercase">
          no pack
        </div>
        <CoverImage
          src={s.attachedPack?.coverImage}
          className="absolute inset-0 w-full h-full object-cover group-hover:scale-105 transition-transform duration-500"
        />
        <div className="absolute inset-0 bg-gradient-to-t from-[var(--color-bg-panel)] via-transparent to-transparent" />
        {/* Region pill */}
        <span className="absolute top-2 left-2 inline-flex items-center text-[10px] tracking-[0.14em] uppercase px-2 py-0.5 rounded bg-[var(--color-bg-page)]/70 backdrop-blur-sm text-[var(--color-text-bright)]/90 ring-1 ring-[var(--color-bg-raised)]/40">
          {REGION_LABEL[s.region] ?? s.region}
        </span>
        {/* Glowy online pill, top-right */}
        {s.online ? (
          <span className="absolute top-2 right-2 inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-[var(--color-status-success)]/15 backdrop-blur-sm text-[10px] font-medium text-[var(--color-status-success)] ring-1 ring-[var(--color-status-success)]/40 shadow-[0_0_14px_rgba(80,200,120,0.45)]">
            <span className="size-1.5 rounded-full bg-[var(--color-status-success)] animate-pulse shadow-[0_0_6px_rgba(80,200,120,0.9)]" />
            Online
          </span>
        ) : (
          <span className="absolute top-2 right-2 inline-flex items-center gap-1 px-2 py-0.5 rounded-full bg-[var(--color-bg-page)]/70 backdrop-blur-sm text-[10px] text-[var(--color-text-dim)] ring-1 ring-[var(--color-bg-raised)]/40">
            <span className="size-1.5 rounded-full bg-[var(--color-text-dim)]/70" />
            Offline
          </span>
        )}
      </div>
      <div className="p-4">
        <div className="flex items-center gap-2 mb-1">
          <span className="font-medium text-[var(--color-text-bright)] truncate flex-1 min-w-0">
            {s.name}
          </span>
          {s.favoriteCount > 0 && (
            <span
              className="shrink-0 inline-flex items-center gap-1 text-[10px] text-[var(--color-text-dim)] tabular-nums"
              title={`${s.favoriteCount} favorite${s.favoriteCount === 1 ? "" : "s"}`}
            >
              <HeartGlyph filled={false} />
              {s.favoriteCount.toLocaleString()}
            </span>
          )}
        </div>
        {s.attachedPack ? (
          <div className="text-[10px] text-[var(--color-text-dim)] mb-2 truncate">
            Running{" "}
            <span className="text-[var(--color-accent-soft)]">
              {s.attachedPack.name}
            </span>
            {s.attachedPack.attachedVersion && (
              <span className="font-mono">
                {" "}
                · v{s.attachedPack.attachedVersion}
              </span>
            )}
          </div>
        ) : (
          <div className="text-[10px] text-[var(--color-text-dim)] mb-2 italic">
            No pack attached
          </div>
        )}
        <p className="text-xs text-[var(--color-text-dim)] leading-relaxed line-clamp-2 min-h-[2.5em]">
          {s.summary ?? ""}
        </p>
        <div className="mt-3 flex items-center justify-between gap-2">
          {/* Players: warning-tinted when full, neutral otherwise.
              Plain text when offline (no live count is meaningful). */}
          <span
            className={`inline-flex items-center px-2 py-0.5 rounded text-[10px] font-medium tabular-nums ring-1 ${
              !s.online
                ? "bg-transparent text-[var(--color-text-dim)] ring-[var(--color-bg-raised)]"
                : isFull
                  ? "bg-[var(--color-status-warning)]/10 text-[var(--color-status-warning)] ring-[var(--color-status-warning)]/40"
                  : "bg-[var(--color-bg-raised)]/40 text-[var(--color-text-bright)]/85 ring-[var(--color-bg-raised)]"
            }`}
          >
            {s.online
              ? isFull
                ? "Full"
                : `${s.currentPlayers}/${s.maxPlayers} players`
              : "Offline"}
          </span>
          <span
            className={`inline-flex items-center px-2 py-0.5 rounded text-[10px] font-medium tabular-nums ring-1 ${ut.bg} ${ut.text} ${ut.ring}`}
          >
            {s.uptimePct.toFixed(0)}% uptime
          </span>
        </div>
      </div>
    </button>
  );
}

function ServerDetailView({
  server,
  attachedPack,
  installedRecord,
  favorited,
  signedIn,
  onToggleFavorite,
  onSignInRequest,
  onBack,
  onInstall,
}: {
  server: CatalogServer;
  /** Full CatalogPack for the attached pack — null if the server
   *  has no pack OR the catalog hasn't loaded yet (we can't install
   *  what we don't have full metadata for). */
  attachedPack: CatalogPack | null;
  /** Install-history record for the attached pack, if any. Drives
   *  the bottom-card variant — "Connect" when versions align,
   *  "Update & connect" when we're behind, "Install" otherwise. */
  installedRecord: InstallRecord | null;
  favorited: boolean;
  signedIn: boolean;
  onToggleFavorite: () => void;
  onSignInRequest: () => void;
  onBack: () => void;
  onInstall: (pack: CatalogPack) => void;
}) {
  const cover = server.attachedPack?.coverImage;

  // Three-state truth table for the bottom card:
  //   not-installed → install (existing behavior)
  //   needs-update  → user has the pack but at the wrong version
  //   current       → the installed version is correct; offer
  //                   direct Connect.
  //
  // "Wrong version" prefers the server's pinned attachedVersion
  // (what THIS server is running). When the server hasn't pinned
  // a version we fall back to the catalog's latest — the server
  // probably auto-updates to it.
  const targetVersion =
    server.attachedPack?.attachedVersion ?? attachedPack?.latestVersion ?? null;
  const installState: "not-installed" | "needs-update" | "current" = (() => {
    if (!installedRecord) return "not-installed";
    if (targetVersion && installedRecord.version !== targetVersion) {
      return "needs-update";
    }
    return "current";
  })();
  return (
    <div className="px-6 py-8 max-w-2xl mx-auto">
      <div className="flex items-center justify-between gap-3 mb-4">
        <button
          type="button"
          onClick={onBack}
          className="text-[10px] tracking-[0.18em] uppercase text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)]"
        >
          ← SERVERS
        </button>
        <HeartButton
          count={server.favoriteCount}
          favorited={favorited}
          signedIn={signedIn}
          onToggle={onToggleFavorite}
          onSignInRequest={onSignInRequest}
        />
      </div>

      <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)] overflow-hidden mb-6">
        <div className="aspect-[16/9] bg-[var(--color-bg-raised)] relative">
          <div className="absolute inset-0 flex items-center justify-center text-[var(--color-text-dim)]/50 text-xs tracking-[0.2em] uppercase">
            no pack
          </div>
          <CoverImage
            src={cover}
            className="absolute inset-0 w-full h-full object-cover"
          />
          <div className="absolute inset-0 bg-gradient-to-t from-[var(--color-bg-panel)] via-[var(--color-bg-panel)]/40 to-transparent" />
          <span className="absolute top-3 left-3 inline-flex items-center text-[10px] tracking-[0.14em] uppercase px-2 py-1 rounded bg-[var(--color-bg-page)]/70 backdrop-blur-sm text-[var(--color-text-bright)]/90 ring-1 ring-[var(--color-bg-raised)]/40">
            {REGION_LABEL[server.region] ?? server.region}
          </span>
          {server.online ? (
            <span className="absolute top-3 right-3 inline-flex items-center gap-1.5 px-2.5 py-1 rounded-full bg-[var(--color-status-success)]/15 backdrop-blur-sm text-[11px] font-medium text-[var(--color-status-success)] ring-1 ring-[var(--color-status-success)]/40 shadow-[0_0_18px_rgba(80,200,120,0.45)]">
              <span className="size-1.5 rounded-full bg-[var(--color-status-success)] animate-pulse shadow-[0_0_6px_rgba(80,200,120,0.9)]" />
              Online
            </span>
          ) : (
            <span className="absolute top-3 right-3 inline-flex items-center gap-1.5 px-2.5 py-1 rounded-full bg-[var(--color-bg-page)]/70 backdrop-blur-sm text-[11px] text-[var(--color-text-dim)] ring-1 ring-[var(--color-bg-raised)]/40">
              <span className="size-1.5 rounded-full bg-[var(--color-text-dim)]/70" />
              Offline
            </span>
          )}
          <div className="absolute bottom-4 left-5 right-5">
            <div className="text-xl font-semibold drop-shadow-lg">
              {server.name}
            </div>
            <div className="text-xs text-[var(--color-text-dim)] mt-1">
              {server.online
                ? `${server.currentPlayers}/${server.maxPlayers} players`
                : "Offline"}
              {" · "}
              {server.uptimePct.toFixed(0)}% uptime (7d)
            </div>
          </div>
        </div>
        {server.summary && (
          <p className="px-5 py-4 text-sm text-[var(--color-text-bright)]/85 leading-relaxed border-t border-[var(--color-bg-raised)]">
            {server.summary}
          </p>
        )}
      </div>

      {server.connectAddress && (
        <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 p-5 mb-4">
          <div className="text-[10px] font-medium tracking-[0.14em] uppercase text-[var(--color-text-bright)]/85 mb-2">
            Connect address
          </div>
          <ConnectCopy address={server.connectAddress} />
        </div>
      )}

      {server.attachedPack ? (
        attachedPack ? (
          <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 p-5">
            <div className="text-[10px] font-medium tracking-[0.14em] uppercase text-[var(--color-text-bright)]/85 mb-2">
              {installState === "current"
                ? "Ready to play"
                : installState === "needs-update"
                  ? "Update available"
                  : "Install the pack"}
            </div>
            <div className="flex items-start gap-3 mb-4">
              <div className="size-14 rounded-md bg-[var(--color-bg-raised)] shrink-0 relative overflow-hidden">
                <CoverImage
                  src={attachedPack.coverImage}
                  className="absolute inset-0 w-full h-full object-cover"
                />
              </div>
              <div className="min-w-0 flex-1">
                <div className="text-sm font-medium text-[var(--color-text-bright)] truncate flex items-center gap-1.5 flex-wrap">
                  {attachedPack.name}
                  {targetVersion && (
                    <span className="font-mono text-[11px] text-[var(--color-text-dim)]">
                      v{targetVersion}
                    </span>
                  )}
                  {installState === "current" && (
                    <span className="inline-flex items-center gap-1 px-1.5 py-0.5 rounded text-[9px] tracking-[0.14em] uppercase font-medium bg-[var(--color-status-success)]/15 text-[var(--color-status-success)] ring-1 ring-[var(--color-status-success)]/40">
                      <span className="size-1 rounded-full bg-[var(--color-status-success)]" />
                      Installed
                    </span>
                  )}
                  {installState === "needs-update" && installedRecord && (
                    <span
                      title={`Installed v${installedRecord.version}`}
                      className="inline-flex items-center px-1.5 py-0.5 rounded text-[9px] tracking-[0.14em] uppercase font-medium bg-[var(--color-accent)]/15 text-[var(--color-accent-soft)] ring-1 ring-[var(--color-accent-soft)]/40"
                    >
                      v{installedRecord.version} → v{targetVersion}
                    </span>
                  )}
                </div>
                <div className="text-[11px] text-[var(--color-text-dim)] mt-0.5">
                  {attachedPack.fileCount.toLocaleString()} files ·{" "}
                  {formatBytes(attachedPack.totalSizeBytes)} · by{" "}
                  {attachedPack.publisherName}
                </div>
              </div>
            </div>

            {installState === "current" && server.connectAddress ? (
              // Already installed at the right version: skip the
              // install flow entirely and offer one-click connect.
              // The button copies the address (clipboard fallback)
              // and asks launch_game to spawn the client directly
              // into the server.
              <ConnectButton address={server.connectAddress} />
            ) : (
              <>
                <button
                  type="button"
                  onClick={() => onInstall(attachedPack)}
                  className="w-full inline-flex items-center justify-center gap-2 px-5 py-2.5 rounded-md bg-[var(--color-accent)] hover:bg-[var(--color-accent)]/90 transition-colors text-sm font-medium"
                >
                  {installState === "needs-update"
                    ? `Update to v${targetVersion ?? "?"} & connect`
                    : "Install pack & get connect address"}
                </button>
                <p className="mt-2 text-[11px] text-[var(--color-text-dim)] leading-relaxed">
                  {installState === "needs-update"
                    ? "Smart update only refetches the changed files. We'll surface the direct-connect address when it lands."
                    : "We'll lay down the pack files, then surface the direct-connect address so you can paste it into the 7DTD launcher."}
                </p>
              </>
            )}
          </div>
        ) : (
          <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 p-5 text-sm text-[var(--color-text-dim)]">
            Loading pack metadata…
          </div>
        )
      ) : (
        <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 p-5 text-sm text-[var(--color-text-dim)]">
          This server doesn&apos;t have a pack attached — connect to
          it directly with the address above.
        </div>
      )}

      {(server.discordUrl || server.websiteUrl) && (
        <div className="mt-4 flex gap-2">
          {server.discordUrl && (
            <a
              href={server.discordUrl}
              target="_blank"
              rel="noopener noreferrer"
              className="inline-flex items-center gap-1.5 px-3 py-1.5 rounded-md border border-[var(--color-bg-raised)] hover:border-[var(--color-accent-soft)]/40 hover:text-[var(--color-text-bright)] text-[var(--color-text-bright)]/85 text-xs transition-colors"
            >
              Discord ↗
            </a>
          )}
          {server.websiteUrl && (
            <a
              href={server.websiteUrl}
              target="_blank"
              rel="noopener noreferrer"
              className="inline-flex items-center gap-1.5 px-3 py-1.5 rounded-md border border-[var(--color-bg-raised)] hover:border-[var(--color-accent-soft)]/40 hover:text-[var(--color-text-bright)] text-[var(--color-text-bright)]/85 text-xs transition-colors"
            >
              Website ↗
            </a>
          )}
        </div>
      )}
    </div>
  );
}

// Image wrapper that swallows broken-URL errors and falls back to
// whatever placeholder the parent renders behind it. Catalog
// publishers control coverImage URLs — the launcher shouldn't show
// the browser's torn-corner glyph if a publisher's CDN is down or
// the seed data points at a missing asset.
// Heart toggle used on pack + server detail pages. Three visual
// states:
//   signed out  → outline heart, disabled, tooltip "sign in to
//                 favorite". Count is still shown.
//   not favorited → outline heart, clickable.
//   favorited   → filled accent-soft heart, clickable to remove.
//
// The toggle itself happens at App level via togglePackFavorite
// / toggleServerFavorite; this component is purely presentational.
function HeartButton({
  count,
  favorited,
  signedIn,
  onToggle,
  onSignInRequest,
  size = "md",
}: {
  count: number;
  favorited: boolean;
  signedIn: boolean;
  onToggle: () => void;
  onSignInRequest: () => void;
  size?: "sm" | "md";
}) {
  const padding = size === "sm" ? "px-2 py-1" : "px-3 py-1.5";
  const fontSize = size === "sm" ? "text-[10px]" : "text-[11px]";
  return (
    <button
      type="button"
      onClick={signedIn ? onToggle : onSignInRequest}
      title={signedIn ? (favorited ? "Remove from favorites" : "Add to favorites") : "Sign in to favorite"}
      className={`inline-flex items-center gap-1.5 ${padding} rounded-md border tracking-wide transition-colors ${fontSize} tabular-nums ${
        favorited
          ? "border-[var(--color-accent-soft)]/50 bg-[var(--color-accent)]/15 text-[var(--color-accent-soft)] hover:bg-[var(--color-accent)]/25"
          : "border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 text-[var(--color-text-dim)] hover:border-[var(--color-accent-soft)]/30 hover:text-[var(--color-accent-soft)]"
      }`}
    >
      <HeartGlyph filled={favorited} />
      {count.toLocaleString()}
    </button>
  );
}

function HeartGlyph({ filled }: { filled: boolean }) {
  return (
    <svg
      width="12"
      height="12"
      viewBox="0 0 16 16"
      fill={filled ? "currentColor" : "none"}
      stroke="currentColor"
      strokeWidth="1.5"
      aria-hidden="true"
    >
      <path
        d="M8 13.5s-5.5-3.4-5.5-7A2.8 2.8 0 0 1 5.3 3.7c1 0 1.9.5 2.7 1.4.8-.9 1.7-1.4 2.7-1.4a2.8 2.8 0 0 1 2.8 2.8c0 3.6-5.5 7-5.5 7z"
        strokeLinejoin="round"
      />
    </svg>
  );
}

/**
 * Down-arrow-into-tray glyph used next to install counts. Matches
 * the visual weight + stroke of the HeartGlyph + the SVG glyphs
 * over on the cloud's /browse page (DownloadGlyph there is the
 * same shape).
 */
function FeaturedStarGlyph() {
  // Matches the cloud /browse Featured chip — filled star icon
  // sized to sit inside a 9px-line-height label without overflowing.
  return (
    <svg
      width="10"
      height="10"
      viewBox="0 0 16 16"
      fill="currentColor"
      aria-hidden="true"
    >
      <path d="M8 1.5l2.06 4.17 4.6.67-3.33 3.25.79 4.58L8 11.99l-4.12 2.17.79-4.58L1.34 6.34l4.6-.67L8 1.5z" />
    </svg>
  );
}

function DownloadGlyph() {
  return (
    <svg
      width="12"
      height="12"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {/* Arrow body */}
      <path d="M8 2.5v8" />
      {/* Arrow head */}
      <path d="M4.75 7.25 8 10.5l3.25-3.25" />
      {/* Tray */}
      <path d="M2.75 12.5h10.5" />
    </svg>
  );
}

function CoverImage({
  src,
  className,
  alt = "",
}: {
  src: string | null | undefined;
  className?: string;
  alt?: string;
}) {
  const [errored, setErrored] = useState(false);
  if (!src || errored) return null;
  return (
    <img
      src={src}
      alt={alt}
      className={className}
      onError={() => setErrored(true)}
    />
  );
}

function ConnectCopy({ address }: { address: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="flex items-center justify-between gap-3">
      <code className="font-mono text-sm text-[var(--color-text-bright)] break-all">
        {address}
      </code>
      <button
        type="button"
        onClick={async () => {
          try {
            await navigator.clipboard.writeText(address);
            setCopied(true);
            setTimeout(() => setCopied(false), 1500);
          } catch {
            // Clipboard API not available — visible text is still selectable.
          }
        }}
        className="shrink-0 inline-flex items-center gap-1.5 px-2.5 py-1 rounded text-[11px] tracking-wide uppercase border border-[var(--color-bg-raised)] hover:border-[var(--color-accent-soft)]/40 hover:text-[var(--color-accent-soft)] text-[var(--color-text-dim)] transition-colors"
      >
        {copied ? "Copied" : "Copy"}
      </button>
    </div>
  );
}

// Single-button "Connect" affordance used in the ServerDetailView
// when the attached pack is already installed at the correct
// version — at that point the install card is just noise and what
// the user wants is to jump straight into the server.
//
// Same launch_game invocation as LaunchPanel under the hood (so
// it picks up the direct-spawn-with-args fast path when we can
// find 7DaysToDie.exe, and falls through to the Steam URI when
// we can't). The address is also primed to clipboard before the
// launch so any arg-stripping at the Steam layer still leaves
// the user one paste away from joining.
function ConnectButton({ address }: { address: string }) {
  const [state, setState] = useState<
    | { kind: "idle" }
    | { kind: "launching" }
    | { kind: "launched" }
    | { kind: "error"; message: string }
  >({ kind: "idle" });

  const connect = useCallback(async () => {
    setState({ kind: "launching" });
    try {
      await navigator.clipboard.writeText(address);
    } catch {
      // Non-fatal — address stays visible in the Connect Address
      // card above. The user can manually copy if needed.
    }
    try {
      await invoke("launch_game", { connectAddress: address });
      setState({ kind: "launched" });
    } catch (e) {
      setState({ kind: "error", message: String(e) });
    }
  }, [address]);

  return (
    <div className="space-y-2">
      <button
        type="button"
        onClick={connect}
        disabled={state.kind === "launching"}
        className="w-full inline-flex items-center justify-center gap-2 px-5 py-2.5 rounded-md bg-[var(--color-accent)] hover:bg-[var(--color-accent)]/90 disabled:opacity-50 disabled:cursor-not-allowed transition-colors text-sm font-medium"
      >
        {state.kind === "launching" ? "Launching…" : "Connect"}
      </button>
      {state.kind === "idle" && (
        <p className="text-[11px] text-[var(--color-text-dim)] leading-relaxed">
          Launches 7DTD and joins this server. If the auto-connect
          drops you on the main menu, the address is on your
          clipboard for a manual paste.
        </p>
      )}
      {state.kind === "launched" && (
        <p className="text-[11px] text-[var(--color-text-dim)] leading-relaxed">
          7DTD is launching with the connect args. If the menu
          loads instead of the server, paste the address into{" "}
          <span className="text-[var(--color-text-bright)]">
            Join a Game → Connect to IP
          </span>
          .
        </p>
      )}
      {state.kind === "error" && (
        <p className="text-[11px] text-[var(--color-status-danger)] leading-relaxed break-words">
          Couldn&apos;t launch 7DTD: {state.message}. Address is on
          your clipboard for a manual launch.
        </p>
      )}
    </div>
  );
}

// One-click launch: copy the address to clipboard, then ask Steam
// to spin up 7DTD via its rungameid URI. The user still has to
// paste the address into Join Game → Connect to IP — 7DTD's
// client doesn't accept a connect address from the command line,
// so the launcher's job stops at "Steam is opening".
function LaunchPanel({ address }: { address: string }) {
  const [state, setState] = useState<
    | { kind: "idle" }
    | { kind: "launching" }
    | { kind: "launched" }
    | { kind: "error"; message: string }
  >({ kind: "idle" });

  const launch = useCallback(async () => {
    setState({ kind: "launching" });
    // Prime the clipboard first — even if Steam fails to open,
    // the user can paste the address into a manually-opened game.
    try {
      await navigator.clipboard.writeText(address);
    } catch {
      // Non-fatal — the address is still visible and selectable.
    }
    try {
      // Pass the connect address through so the Rust side can ask
      // Steam to forward -connecttoip / -connecttoport to the
      // client. Steam occasionally strips args (varies by version),
      // so the clipboard prime above is the reliable fallback.
      await invoke("launch_game", { connectAddress: address });
      setState({ kind: "launched" });
    } catch (e) {
      setState({ kind: "error", message: String(e) });
    }
  }, [address]);

  return (
    <div className="space-y-3">
      <ConnectCopy address={address} />
      <button
        type="button"
        onClick={launch}
        disabled={state.kind === "launching"}
        className="w-full inline-flex items-center justify-center gap-2 px-5 py-2.5 rounded-md bg-[var(--color-accent)] hover:bg-[var(--color-accent)]/90 disabled:opacity-50 disabled:cursor-not-allowed transition-colors text-sm font-medium"
      >
        {state.kind === "launching" ? "Opening Steam…" : "Launch 7DTD"}
      </button>
      {state.kind === "launched" && (
        <p className="text-[11px] text-[var(--color-text-dim)] leading-relaxed">
          Steam is launching 7DTD with the connect args. If it lands
          on the main menu instead, the address is on your clipboard —
          paste it into <span className="text-[var(--color-text-bright)]">Join a Game → Connect to IP</span>.
        </p>
      )}
      {state.kind === "error" && (
        <p className="text-[11px] text-[var(--color-status-danger)] leading-relaxed">
          Couldn&apos;t open Steam: {state.message}. The address is
          still on your clipboard — launch 7DTD manually and paste it
          into Join a Game.
        </p>
      )}
      {state.kind === "idle" && (
        <p className="text-[11px] text-[var(--color-text-dim)] leading-relaxed">
          Asks Steam to launch 7DTD and connect to this server. If
          Steam strips the connect args the address is on your
          clipboard for a manual paste.
        </p>
      )}
    </div>
  );
}

function FolderGlyph() {
  return (
    <svg
      width="14"
      height="14"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.6"
      aria-hidden="true"
    >
      <path
        d="M2 5a1 1 0 0 1 1-1h3l1.5 1.5H13a1 1 0 0 1 1 1v5a1 1 0 0 1-1 1H3a1 1 0 0 1-1-1V5z"
        strokeLinejoin="round"
      />
    </svg>
  );
}

// Dashboard landing — the first view a user sees on launch.
// Reuses the kitsune brand mark as a visual anchor, surfaces a
// short "what you can do here" pitch, and shows recent installs
// + a sign-in CTA when signed out. Lightweight on purpose — the
// real Dashboard fleshes out in follow-up rounds once metrics
// and notifications have backend support.
function DashboardView({
  auth,
  history,
  packBySlug,
  onBrowse,
  onOpenPack,
  onSignIn,
}: {
  auth: AuthState;
  history: InstallRecord[];
  packBySlug: Map<string, CatalogPack>;
  onBrowse: () => void;
  onOpenPack: (p: CatalogPack) => void;
  onSignIn: () => void;
}) {
  const greeting =
    auth.kind === "signedIn"
      ? `Welcome back, ${auth.user.displayName}`
      : "Welcome to PackRelay";
  const tagline =
    auth.kind === "signedIn"
      ? "Ready to sync, survivor?"
      : "Sign in to save your library + servers to your account.";
  const recent = history.slice(0, 4);

  return (
    <div className="px-6 py-8 max-w-5xl mx-auto">
      <div className="mb-6">
        <div className="text-[10px] tracking-[0.18em] uppercase text-[var(--color-text-dim)]">
          {greeting.split(",")[0].toUpperCase()}
        </div>
        <h1 className="text-2xl font-semibold tracking-tight mt-1">
          {greeting}
        </h1>
        <p className="text-sm text-[var(--color-text-dim)] mt-1">{tagline}</p>
      </div>

      {/* Hero card with the kitsune mark on the right. The grid
          collapses to a single column on narrower viewports; the
          mark is decorative so it just disappears below the text. */}
      <div className="relative overflow-hidden rounded-2xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)] mb-8">
        <div className="grid grid-cols-1 sm:grid-cols-[1fr_auto] gap-6 items-center px-6 sm:px-8 py-7">
          <div className="min-w-0">
            <div className="text-[10px] tracking-[0.22em] uppercase text-[var(--color-accent-soft)] mb-2">
              Connect · Sync · Survive
            </div>
            <h2 className="text-xl font-semibold tracking-tight mb-2 text-[var(--color-text-bright)]">
              Signed pack delivery for 7DTD.
            </h2>
            <p className="text-sm text-[var(--color-text-dim)] leading-relaxed max-w-md mb-5">
              Browse a catalog of community-built modpacks, install
              into your Mods/ directory in one click, and jump into a
              server with the connect address already on your
              clipboard.
            </p>
            <div className="flex flex-wrap gap-2">
              <button
                type="button"
                onClick={onBrowse}
                className="inline-flex items-center gap-2 px-4 py-2 rounded-md bg-[var(--color-accent)] hover:bg-[var(--color-accent)]/90 transition-colors text-sm font-medium"
              >
                Browse packs
              </button>
              {auth.kind === "signedOut" && (
                <button
                  type="button"
                  onClick={onSignIn}
                  className="inline-flex items-center gap-2 px-4 py-2 rounded-md border border-[var(--color-bg-raised)] hover:border-[var(--color-accent-soft)]/40 hover:text-[var(--color-text-bright)] text-[var(--color-text-bright)]/85 text-sm font-medium transition-colors"
                >
                  Sign in
                </button>
              )}
            </div>
          </div>
          <img
            src="/logo-512x512.png"
            alt=""
            className="hidden sm:block size-40 select-none opacity-90"
            draggable={false}
          />
        </div>
      </div>

      {recent.length > 0 && (
        <div className="mb-8">
          <div className="flex items-end justify-between mb-3">
            <h2 className="text-sm font-semibold tracking-tight">
              Recently installed
            </h2>
            <button
              type="button"
              onClick={onBrowse}
              className="text-[10px] tracking-[0.14em] uppercase text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)] transition-colors"
            >
              Browse more →
            </button>
          </div>
          <ul className="grid grid-cols-2 sm:grid-cols-4 gap-3">
            {recent.map((r) => {
              const pack = packBySlug.get(r.slug);
              return (
                <li key={`${r.slug}-${r.installedAt}`}>
                  <button
                    type="button"
                    onClick={() => pack && onOpenPack(pack)}
                    disabled={!pack}
                    className="w-full text-left rounded-lg border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 overflow-hidden hover:border-[var(--color-accent-soft)]/40 disabled:opacity-60 disabled:cursor-not-allowed transition-colors"
                  >
                    <div className="aspect-[16/9] bg-[var(--color-bg-raised)] relative overflow-hidden">
                      <div className="absolute inset-0 flex items-center justify-center text-[var(--color-text-dim)]/40 text-[10px] tracking-[0.2em] uppercase">
                        no cover
                      </div>
                      <CoverImage
                        src={pack?.coverImage}
                        className="absolute inset-0 w-full h-full object-cover"
                      />
                    </div>
                    <div className="px-3 py-2">
                      <div className="text-[12px] font-medium text-[var(--color-text-bright)] truncate">
                        {r.name}
                      </div>
                      <div className="text-[10px] text-[var(--color-text-dim)] font-mono mt-0.5">
                        v{r.version}
                      </div>
                    </div>
                  </button>
                </li>
              );
            })}
          </ul>
        </div>
      )}
    </div>
  );
}

// Settings — placeholder for now. Surfaces the auth chip's
// info inline + sign-out button, since the rail's auth menu is
// the only formal sign-out path today. Future iterations: API
// URL toggle, default install dest override, theme picker.
function SettingsView({
  auth,
  onSignOut,
}: {
  auth: AuthState;
  onSignOut: () => void;
}) {
  return (
    <div className="px-6 py-8 max-w-2xl space-y-4">
      <div className="mb-2">
        <h1 className="text-xl font-semibold tracking-tight">Settings</h1>
        <p className="text-xs text-[var(--color-text-dim)] mt-1">
          More options land here as the launcher grows — API URL toggle,
          default install path, theme.
        </p>
      </div>

      <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 p-5">
        <h2 className="text-[10px] font-medium tracking-[0.14em] uppercase text-[var(--color-text-bright)]/85 mb-3">
          Account
        </h2>
        {auth.kind === "signedIn" ? (
          <div className="space-y-2">
            <div className="text-sm text-[var(--color-text-bright)]">
              Signed in as{" "}
              <span className="font-medium">{auth.user.displayName}</span>
            </div>
            <div className="text-[11px] text-[var(--color-text-dim)] uppercase tracking-wide">
              {auth.user.role.replace("_", " ")} · {auth.user.plan} plan
            </div>
            <button
              type="button"
              onClick={onSignOut}
              className="mt-3 inline-flex items-center px-3 py-1.5 rounded-md border border-[var(--color-bg-raised)] hover:border-[var(--color-status-danger)]/50 hover:text-[var(--color-status-danger)] text-[var(--color-text-bright)]/85 text-[11px] tracking-[0.14em] uppercase transition-colors"
            >
              Sign out
            </button>
          </div>
        ) : (
          <p className="text-sm text-[var(--color-text-dim)]">
            Sign in via the chip at the bottom of the left rail to save
            your library + servers to your packrelay.cloud account.
          </p>
        )}
      </div>

      <CacheSection />
    </div>
  );
}

// Wire types match blob_cache::{CacheStats, GcResult}. Numbers come
// over the wire as JSON numbers — u64 fits inside JS's safe-integer
// range for any plausible cache size (≈9 petabytes before precision
// breaks), so number is fine here.
type CacheStats = {
  totalBlobs: number;
  totalBytes: number;
  referencedBlobs: number;
  unreferencedBlobs: number;
  reclaimableBytes: number;
  // RFC3339; null when the launcher has never swept this cache.
  // Set by both the manual "Clean cache" button and the weekly
  // background sweep that fires from app::setup.
  lastSweepAt: string | null;
};
type GcResult = {
  blobsRemoved: number;
  bytesFreed: number;
};

// Cache disk-usage card for the Settings page. Shows the totals
// straight from packrelay-core's cache_stats command, plus a
// destructive button to GC unreferenced blobs.
//
// "Unreferenced" = no profile sidecar lists that blob's hash. Blobs
// that were installed and uninstalled accumulate here (uninstall
// only clears the live 7DTD copy, never the cache) and so do blobs
// from older pack versions that profiles no longer pin. Clicking
// Clean cache walks the cache and removes everything not pinned.
function CacheSection() {
  const [stats, setStats] = useState<CacheStats | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [cleaning, setCleaning] = useState(false);
  const [lastResult, setLastResult] = useState<GcResult | null>(null);

  const refresh = useCallback(async () => {
    try {
      const s = await invoke<CacheStats>("cache_stats");
      setStats(s);
      setError(null);
    } catch (e) {
      setError(typeof e === "string" ? e : `${e}`);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const reclaimable = stats?.reclaimableBytes ?? 0;
  const canClean = !!stats && stats.unreferencedBlobs > 0 && !cleaning;

  return (
    <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 p-5">
      <div className="flex items-baseline justify-between gap-3 mb-1">
        <h2 className="text-[10px] font-medium tracking-[0.14em] uppercase text-[var(--color-text-bright)]/85">
          Cache
        </h2>
        {stats?.lastSweepAt && (
          <span
            className="text-[10px] text-[var(--color-text-dim)] tabular-nums"
            title={`Last cleaned at ${stats.lastSweepAt}`}
          >
            Last cleaned {formatRelativeTime(stats.lastSweepAt)}
          </span>
        )}
      </div>
      <p className="text-[11px] text-[var(--color-text-dim)] leading-relaxed mb-4">
        Every file you install is content-addressed and stored once
        in a shared blob cache — that&apos;s how profile switching
        stays fast (hardlinks, not copies). Uninstalled packs and
        older pack versions stick around here until a deliberate
        sweep, so switching back never has to re-download. The
        launcher also auto-sweeps weekly in the background.
      </p>

      {stats === null && error === null && (
        <div className="text-[11px] text-[var(--color-text-dim)]">
          Reading cache…
        </div>
      )}
      {error && (
        <div className="mb-3 rounded-md border border-[var(--color-status-danger)]/40 bg-[var(--color-status-danger)]/10 px-3 py-2 text-[11px] text-[var(--color-status-danger)]">
          {error}
        </div>
      )}
      {stats && (
        <>
          <div className="grid grid-cols-3 gap-2 mb-4 text-[11px]">
            <CacheStat
              label="On disk"
              value={formatBytes(stats.totalBytes)}
              sub={`${stats.totalBlobs} file${stats.totalBlobs === 1 ? "" : "s"}`}
            />
            <CacheStat
              label="In use"
              value={`${stats.referencedBlobs}`}
              sub="pinned by a profile"
            />
            <CacheStat
              label="Reclaimable"
              value={formatBytes(stats.reclaimableBytes)}
              sub={`${stats.unreferencedBlobs} unreferenced`}
              highlight={stats.unreferencedBlobs > 0}
            />
          </div>
          <div className="flex items-center gap-3 flex-wrap">
            <button
              type="button"
              disabled={!canClean}
              onClick={async () => {
                const ok = await ask(
                  `Free ${formatBytes(reclaimable)} by removing ${stats.unreferencedBlobs} cached file${stats.unreferencedBlobs === 1 ? "" : "s"} no profile currently pins?\n\nProfiles you can still switch to keep all of their files. Removed files re-download on demand if you ever pin them again.`,
                  { title: "Clean cache", kind: "warning" }
                );
                if (!ok) return;
                setCleaning(true);
                setError(null);
                setLastResult(null);
                try {
                  const r = await invoke<GcResult>("cache_gc");
                  setLastResult(r);
                  await refresh();
                } catch (e) {
                  setError(typeof e === "string" ? e : `${e}`);
                } finally {
                  setCleaning(false);
                }
              }}
              className="inline-flex items-center px-3 py-1.5 rounded-md bg-[var(--color-accent)] hover:bg-[var(--color-accent)]/90 disabled:opacity-40 disabled:cursor-not-allowed transition-colors text-[11px] tracking-[0.14em] uppercase font-medium"
            >
              {cleaning ? "Cleaning…" : "Clean cache"}
            </button>
            {lastResult && (
              <span className="text-[11px] text-[var(--color-status-success)]">
                Freed {formatBytes(lastResult.bytesFreed)} ·{" "}
                {lastResult.blobsRemoved} file
                {lastResult.blobsRemoved === 1 ? "" : "s"} removed
              </span>
            )}
            {stats.unreferencedBlobs === 0 && !lastResult && (
              <span className="text-[11px] text-[var(--color-text-dim)]">
                Nothing to clean — every cached file is pinned by a
                profile.
              </span>
            )}
          </div>
        </>
      )}
    </div>
  );
}

function CacheStat({
  label,
  value,
  sub,
  highlight,
}: {
  label: string;
  value: string;
  sub: string;
  highlight?: boolean;
}) {
  return (
    <div className="rounded-md border border-[var(--color-bg-raised)] bg-[var(--color-bg-page)]/40 px-2.5 py-2">
      <div className="text-[9px] font-medium tracking-[0.14em] uppercase text-[var(--color-text-dim)]">
        {label}
      </div>
      <div
        className={`text-[13px] font-semibold tabular-nums mt-0.5 ${
          highlight
            ? "text-[var(--color-accent-soft)]"
            : "text-[var(--color-text-bright)]"
        }`}
      >
        {value}
      </div>
      <div className="text-[10px] text-[var(--color-text-dim)] mt-0.5">
        {sub}
      </div>
    </div>
  );
}

// Profile management surface. Two life-cycle states:
//
//   uninitialized — no profiles exist yet. Render onboarding card
//     that imports the user's current 7DTD setup as the first
//     profile.
//   initialized — show the profile list, with create / switch /
//     rename / delete affordances + snapshot history per row.
//
// All ops route through the Tauri commands; the view holds local
// optimistic state for the list so switches feel instant even
// when the underlying copy is slow.
function ProfilesView() {
  const [initial, setInitial] = useState<ProfileInitialState | null>(null);
  const [profiles, setProfiles] = useState<ProfileSummary[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [snapshotsForId, setSnapshotsForId] = useState<string | null>(null);
  const [snapshots, setSnapshots] = useState<ProfileSnapshot[] | null>(null);

  const refresh = useCallback(async () => {
    try {
      const [state, list] = await Promise.all([
        invoke<ProfileInitialState>("profile_initial_state"),
        invoke<ProfileSummary[]>("profile_list"),
      ]);
      setInitial(state);
      setProfiles(list);
    } catch (e) {
      setError(typeof e === "string" ? e : `${e}`);
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  if (initial === null || profiles === null) {
    return (
      <div className="px-6 py-12 text-sm text-[var(--color-text-dim)]">
        Loading profiles…
      </div>
    );
  }

  if (initial.kind === "uninitialized") {
    return (
      <ProfilesOnboarding
        suggestedUserdataDir={initial.suggestedUserdataDir}
        onImported={() => {
          setInitial(null);
          setProfiles(null);
          void refresh();
        }}
      />
    );
  }

  return (
    <div className="px-6 py-8">
      <div className="mb-5 flex items-end justify-between gap-4 flex-wrap">
        <div>
          <div className="text-[10px] tracking-[0.22em] uppercase text-[var(--color-accent-soft)] mb-1">
            Multiple worlds, one launcher
          </div>
          <h1 className="text-xl font-semibold tracking-tight">Profiles</h1>
          <p className="text-xs text-[var(--color-text-dim)] mt-1">
            Each profile is its own bundle of mods, saves, and worlds.
            Switching swaps all three at once — keep a vanilla profile
            alongside a heavily-modded one, with their saves kept
            separate.
          </p>
        </div>
      </div>

      <NewProfileCard
        onCreated={() => void refresh()}
        onError={setError}
      />

      {error && (
        <div className="mb-4 rounded-md border border-[var(--color-status-danger)]/40 bg-[var(--color-status-danger)]/10 px-3 py-2 text-[11px] text-[var(--color-status-danger)] flex items-start justify-between gap-3">
          <span className="break-words">{error}</span>
          <button
            type="button"
            onClick={() => setError(null)}
            className="shrink-0 text-[10px] tracking-[0.14em] uppercase hover:underline"
          >
            dismiss
          </button>
        </div>
      )}

      <ul className="space-y-3">
        {profiles.map((p) => (
          <li key={p.id}>
            <ProfileCard
              profile={p}
              busy={busy === p.id}
              onSwitch={async () => {
                setBusy(p.id);
                setError(null);
                try {
                  await invoke("profile_switch", { id: p.id });
                  await refresh();
                } catch (e) {
                  setError(typeof e === "string" ? e : `${e}`);
                } finally {
                  setBusy(null);
                }
              }}
              onRename={async (newName) => {
                setError(null);
                try {
                  await invoke("profile_rename", { id: p.id, name: newName });
                  await refresh();
                } catch (e) {
                  setError(typeof e === "string" ? e : `${e}`);
                }
              }}
              onClone={async () => {
                // Default clone name: `<src> (copy)`, auto-bumping
                // the suffix if a name collision exists. The user
                // can rename right after via double-click — same
                // edit flow as any other profile.
                const baseName = `${p.name} (copy)`;
                const existing = new Set(
                  profiles?.map((x) => x.name) ?? []
                );
                let cloneName = baseName;
                let n = 2;
                while (existing.has(cloneName)) {
                  cloneName = `${p.name} (copy ${n})`;
                  n += 1;
                }
                setBusy(p.id);
                setError(null);
                try {
                  await invoke("profile_clone", {
                    id: p.id,
                    name: cloneName,
                  });
                  await refresh();
                } catch (e) {
                  setError(typeof e === "string" ? e : `${e}`);
                } finally {
                  setBusy(null);
                }
              }}
              onDelete={async () => {
                const ok = await ask(
                  `Delete profile "${p.name}"?\n\nRemoves the profile's mods, saves, worlds, and snapshots from disk. The live 7DTD files aren't touched (those belong to whichever profile is currently active).`,
                  { title: "Delete profile", kind: "warning" }
                );
                if (!ok) return;
                setError(null);
                try {
                  await invoke("profile_delete", { id: p.id });
                  await refresh();
                } catch (e) {
                  setError(typeof e === "string" ? e : `${e}`);
                }
              }}
              onShowSnapshots={async () => {
                setSnapshotsForId(p.id);
                setSnapshots(null);
                try {
                  const list = await invoke<ProfileSnapshot[]>(
                    "profile_list_snapshots",
                    { profileId: p.id }
                  );
                  setSnapshots(list);
                } catch (e) {
                  setError(typeof e === "string" ? e : `${e}`);
                  setSnapshotsForId(null);
                }
              }}
            />
          </li>
        ))}
      </ul>

      {snapshotsForId && (
        <SnapshotsModal
          profileId={snapshotsForId}
          profileName={
            profiles.find((p) => p.id === snapshotsForId)?.name ?? "profile"
          }
          snapshots={snapshots}
          isActive={
            profiles.find((p) => p.id === snapshotsForId)?.isActive ?? false
          }
          onRestored={async () => {
            await refresh();
          }}
          onClose={() => {
            setSnapshotsForId(null);
            setSnapshots(null);
          }}
        />
      )}
    </div>
  );
}

// First-run onboarding card. Asks the user to confirm/pick the
// 7DTD userdata dir (we pre-fill our best guess from the OS
// canonical path) and name the imported profile.
function ProfilesOnboarding({
  suggestedUserdataDir,
  onImported,
}: {
  suggestedUserdataDir: string | null;
  onImported: () => void;
}) {
  const [dir, setDir] = useState(suggestedUserdataDir ?? "");
  const [name, setName] = useState("Default");
  const [importing, setImporting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  return (
    <div className="px-6 py-12 max-w-2xl mx-auto">
      <div className="text-[10px] tracking-[0.22em] uppercase text-[var(--color-accent-soft)] mb-2">
        Get started
      </div>
      <h1 className="text-2xl font-semibold tracking-tight mb-3">
        Set up profiles
      </h1>
      <p className="text-sm text-[var(--color-text-dim)] leading-relaxed mb-6">
        A profile is a named bundle of mods, saves, and worlds. PackRelay
        keeps each profile isolated so you can run a heavily-modded
        server alongside a vanilla save without them clobbering each
        other. Switch profiles → mods, saves, and worlds all swap at
        once.
      </p>

      <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)] p-5 space-y-4">
        <div>
          <label
            htmlFor="userdata-dir"
            className="block text-[10px] font-medium tracking-[0.14em] uppercase text-[var(--color-text-bright)]/85 mb-1.5"
          >
            7DTD userdata folder
          </label>
          <input
            id="userdata-dir"
            type="text"
            value={dir}
            onChange={(e) => setDir(e.currentTarget.value)}
            disabled={importing}
            className="w-full rounded-md bg-[var(--color-bg-page)] border border-[var(--color-bg-raised)] px-3 py-2 text-sm font-mono text-[var(--color-text-bright)] outline-none focus:border-[var(--color-accent-soft)]/60 focus:ring-2 focus:ring-[var(--color-accent)]/20 transition-colors disabled:opacity-60"
            placeholder="Path containing Mods/, Saves/, GeneratedWorlds/"
            spellCheck={false}
          />
          <p className="mt-1.5 text-[11px] text-[var(--color-text-dim)] leading-relaxed">
            On Windows the standard path is{" "}
            <code className="font-mono">%APPDATA%\7DaysToDie</code> — the
            parent of the Mods folder you install packs into.
          </p>
        </div>

        <div>
          <label
            htmlFor="profile-name"
            className="block text-[10px] font-medium tracking-[0.14em] uppercase text-[var(--color-text-bright)]/85 mb-1.5"
          >
            Name this profile
          </label>
          <input
            id="profile-name"
            type="text"
            value={name}
            onChange={(e) => setName(e.currentTarget.value)}
            disabled={importing}
            maxLength={64}
            className="w-full rounded-md bg-[var(--color-bg-page)] border border-[var(--color-bg-raised)] px-3 py-2 text-sm text-[var(--color-text-bright)] outline-none focus:border-[var(--color-accent-soft)]/60 focus:ring-2 focus:ring-[var(--color-accent)]/20 transition-colors disabled:opacity-60"
            placeholder="Default"
          />
          <p className="mt-1.5 text-[11px] text-[var(--color-text-dim)] leading-relaxed">
            We&apos;ll snapshot your current Mods + Saves + GeneratedWorlds
            into this profile so nothing's lost. You can rename it later.
          </p>
        </div>

        {error && (
          <div className="rounded-md border border-[var(--color-status-danger)]/40 bg-[var(--color-status-danger)]/10 px-3 py-2 text-[11px] text-[var(--color-status-danger)] break-words">
            {error}
          </div>
        )}

        <button
          type="button"
          disabled={importing || !dir.trim() || !name.trim()}
          onClick={async () => {
            setImporting(true);
            setError(null);
            try {
              await invoke("profile_import_current", {
                userdataDir: dir,
                name,
              });
              onImported();
            } catch (e) {
              setError(typeof e === "string" ? e : `${e}`);
            } finally {
              setImporting(false);
            }
          }}
          className="w-full inline-flex items-center justify-center gap-2 px-5 py-2.5 rounded-md bg-[var(--color-accent)] hover:bg-[var(--color-accent)]/90 disabled:opacity-50 disabled:cursor-not-allowed transition-colors text-sm font-medium"
        >
          {importing ? "Importing…" : "Import current setup as profile"}
        </button>
      </div>
    </div>
  );
}

// Inline "create new profile" form on the profiles list page. Bare
// — name only, no mods/saves picked yet (those come via switching
// to it + installing packs).
function NewProfileCard({
  onCreated,
  onError,
}: {
  onCreated: () => void;
  onError: (msg: string) => void;
}) {
  const [name, setName] = useState("");
  const [creating, setCreating] = useState(false);
  return (
    <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)] p-4 mb-5">
      <form
        onSubmit={async (e) => {
          e.preventDefault();
          if (!name.trim()) return;
          setCreating(true);
          try {
            await invoke("profile_create", { name });
            setName("");
            onCreated();
          } catch (err) {
            onError(typeof err === "string" ? err : `${err}`);
          } finally {
            setCreating(false);
          }
        }}
        className="flex flex-wrap items-end gap-3"
      >
        <div className="flex-1 min-w-[200px]">
          <label
            htmlFor="new-profile-name"
            className="block text-[10px] font-medium tracking-[0.14em] uppercase text-[var(--color-text-bright)]/85 mb-1.5"
          >
            New profile
          </label>
          <input
            id="new-profile-name"
            type="text"
            value={name}
            onChange={(e) => setName(e.currentTarget.value)}
            disabled={creating}
            maxLength={64}
            placeholder="e.g. Hardcore Day 7, Vanilla, Test Build"
            className="w-full rounded-md bg-[var(--color-bg-page)] border border-[var(--color-bg-raised)] px-3 py-2 text-sm text-[var(--color-text-bright)] placeholder:text-[var(--color-text-dim)]/60 outline-none focus:border-[var(--color-accent-soft)]/60 focus:ring-2 focus:ring-[var(--color-accent)]/20 transition-colors disabled:opacity-60"
          />
        </div>
        <button
          type="submit"
          disabled={creating || !name.trim()}
          className="inline-flex items-center px-4 py-2 rounded-md bg-[var(--color-accent)] hover:bg-[var(--color-accent)]/90 disabled:opacity-50 disabled:cursor-not-allowed transition-colors text-sm font-medium"
        >
          {creating ? "Creating…" : "Create"}
        </button>
      </form>
      <p className="mt-2 text-[11px] text-[var(--color-text-dim)] leading-relaxed">
        New profiles start empty. Switch to one, then install a pack —
        that pack lives in this profile and follows it when you switch
        back later.
      </p>
    </div>
  );
}

function ProfileCard({
  profile: p,
  busy,
  onSwitch,
  onRename,
  onClone,
  onDelete,
  onShowSnapshots,
}: {
  profile: ProfileSummary;
  busy: boolean;
  onSwitch: () => void;
  onRename: (newName: string) => Promise<void>;
  onClone: () => void;
  onDelete: () => void;
  onShowSnapshots: () => void;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(p.name);

  const totalBytes = p.modsBytes + p.savesBytes + p.worldsBytes;

  return (
    <div
      className={`rounded-xl border ${
        p.isActive
          ? "border-[var(--color-accent-soft)]/50 bg-[var(--color-accent)]/8"
          : "border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]"
      } p-5 transition-colors`}
    >
      <div className="flex items-start justify-between gap-4 mb-3">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2 mb-1">
            {editing ? (
              <form
                onSubmit={async (e) => {
                  e.preventDefault();
                  if (draft.trim() && draft !== p.name) {
                    await onRename(draft.trim());
                  }
                  setEditing(false);
                }}
                className="flex-1 flex items-center gap-1.5"
              >
                <input
                  autoFocus
                  type="text"
                  value={draft}
                  onChange={(e) => setDraft(e.currentTarget.value)}
                  onBlur={() => {
                    setDraft(p.name);
                    setEditing(false);
                  }}
                  maxLength={64}
                  className="flex-1 min-w-0 rounded-md bg-[var(--color-bg-page)] border border-[var(--color-bg-raised)] px-2.5 py-1 text-sm text-[var(--color-text-bright)] outline-none focus:border-[var(--color-accent-soft)]/60"
                />
              </form>
            ) : (
              <span
                className="text-base font-semibold text-[var(--color-text-bright)] truncate cursor-text"
                onDoubleClick={() => {
                  setDraft(p.name);
                  setEditing(true);
                }}
                title="Double-click to rename"
              >
                {p.name}
              </span>
            )}
            {p.isActive && (
              <span className="shrink-0 inline-flex items-center gap-1 px-2 py-0.5 rounded-full text-[9px] tracking-[0.14em] uppercase font-medium bg-[var(--color-status-success)]/15 text-[var(--color-status-success)] ring-1 ring-[var(--color-status-success)]/40">
                <span className="size-1.5 rounded-full bg-[var(--color-status-success)] shadow-[0_0_6px_rgba(80,200,120,0.7)]" />
                Active
              </span>
            )}
          </div>
          <div className="text-[11px] text-[var(--color-text-dim)] flex flex-wrap items-center gap-x-3 gap-y-0.5">
            {p.packSlug && (
              <span>
                Pack:{" "}
                <span className="text-[var(--color-accent-soft)]">
                  {p.packSlug}
                </span>
                {p.packVersion && (
                  <span className="font-mono"> v{p.packVersion}</span>
                )}
              </span>
            )}
            <span className="font-mono tabular-nums">{formatBytes(totalBytes)}</span>
            <span>
              {p.snapshotCount} snapshot{p.snapshotCount === 1 ? "" : "s"}
            </span>
            <span>created {formatRelativeTime(p.createdAt)}</span>
          </div>
        </div>
        <div className="shrink-0 flex items-center gap-1.5">
          {!p.isActive && (
            <button
              type="button"
              onClick={onSwitch}
              disabled={busy}
              className="inline-flex items-center px-3 py-1.5 rounded-md bg-[var(--color-accent)] hover:bg-[var(--color-accent)]/90 disabled:opacity-50 transition-colors text-[11px] tracking-[0.14em] uppercase font-medium"
            >
              {busy ? "Switching…" : "Switch to"}
            </button>
          )}
          <button
            type="button"
            onClick={onShowSnapshots}
            className="inline-flex items-center px-3 py-1.5 rounded-md border border-[var(--color-bg-raised)] hover:border-[var(--color-accent-soft)]/40 text-[var(--color-text-bright)]/85 text-[11px] tracking-[0.14em] uppercase transition-colors"
          >
            Snapshots
          </button>
          <button
            type="button"
            onClick={() => {
              setDraft(p.name);
              setEditing(true);
            }}
            className="inline-flex items-center px-2.5 py-1.5 rounded-md text-[10px] tracking-wide uppercase text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)] hover:bg-[var(--color-bg-raised)]/40 transition-colors"
            title="Rename"
          >
            ✎
          </button>
          <button
            type="button"
            onClick={onClone}
            disabled={busy}
            className="inline-flex items-center px-2.5 py-1.5 rounded-md text-[10px] tracking-wide uppercase text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)] hover:bg-[var(--color-bg-raised)]/40 disabled:opacity-50 transition-colors"
            title="Duplicate (fork mods + saves + worlds into a new profile)"
          >
            ⎘
          </button>
          {!p.isActive && (
            <button
              type="button"
              onClick={onDelete}
              className="inline-flex items-center px-2.5 py-1.5 rounded-md text-[10px] tracking-wide uppercase text-[var(--color-text-dim)] hover:text-[var(--color-status-danger)] hover:bg-[var(--color-bg-raised)]/40 transition-colors"
              title="Delete"
            >
              ✕
            </button>
          )}
        </div>
      </div>

      <div className="grid grid-cols-3 gap-2 text-[11px]">
        <ProfileStat label="Mods" bytes={p.modsBytes} />
        <ProfileStat label="Saves" bytes={p.savesBytes} />
        <ProfileStat label="Worlds" bytes={p.worldsBytes} />
      </div>
    </div>
  );
}

function ProfileStat({ label, bytes }: { label: string; bytes: number }) {
  return (
    <div className="rounded-md border border-[var(--color-bg-raised)] bg-[var(--color-bg-page)]/40 px-2.5 py-1.5">
      <div className="text-[9px] font-medium tracking-[0.14em] uppercase text-[var(--color-text-dim)]">
        {label}
      </div>
      <div className="text-[12px] font-medium text-[var(--color-text-bright)] tabular-nums mt-0.5">
        {bytes > 0 ? formatBytes(bytes) : "—"}
      </div>
    </div>
  );
}

function SnapshotsModal({
  profileId,
  profileName,
  snapshots,
  isActive,
  onRestored,
  onClose,
}: {
  profileId: string;
  profileName: string;
  snapshots: ProfileSnapshot[] | null;
  isActive: boolean;
  onRestored: () => Promise<void>;
  onClose: () => void;
}) {
  const [restoring, setRestoring] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  return (
    <div
      className="fixed inset-0 z-50 bg-black/60 backdrop-blur-sm flex items-center justify-center px-6"
      onClick={onClose}
    >
      <div
        className="w-full max-w-lg rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)] shadow-2xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="px-5 py-4 border-b border-[var(--color-bg-raised)]/60 flex items-center justify-between">
          <div>
            <h2 className="text-sm font-semibold">Snapshots</h2>
            <p className="text-[11px] text-[var(--color-text-dim)]">
              {profileName} · pre-launch saves + worlds backups
            </p>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)] text-lg leading-none"
            aria-label="Close"
          >
            ×
          </button>
        </div>
        <div className="px-5 py-4">
          {!isActive && (
            <p className="rounded-md border border-[var(--color-status-warning)]/40 bg-[var(--color-status-warning)]/10 px-3 py-2 text-[11px] text-[var(--color-status-warning)] mb-3">
              This profile isn&apos;t active. Switch to it before
              restoring a snapshot — restores write to the live 7DTD
              folders, which only this profile claims when active.
            </p>
          )}
          {snapshots === null ? (
            <div className="text-[11px] text-[var(--color-text-dim)]">
              Loading…
            </div>
          ) : snapshots.length === 0 ? (
            <div className="text-[11px] text-[var(--color-text-dim)] leading-relaxed">
              No snapshots yet. The launcher auto-snapshots saves +
              worlds every time you click <b>Launch 7DTD</b>, keeping
              the last 5.
            </div>
          ) : (
            <ul className="space-y-2">
              {snapshots.map((s) => (
                <li
                  key={s.id}
                  className="rounded-md border border-[var(--color-bg-raised)] bg-[var(--color-bg-page)]/40 px-3 py-2.5 flex items-center justify-between gap-3"
                >
                  <div className="min-w-0">
                    <div className="text-[12px] text-[var(--color-text-bright)] truncate">
                      {s.label ?? "Snapshot"}
                    </div>
                    <div className="text-[10px] text-[var(--color-text-dim)] tabular-nums">
                      {formatRelativeTime(s.createdAt)} ·{" "}
                      {formatBytes(s.savesBytes + s.worldsBytes)}
                    </div>
                  </div>
                  <button
                    type="button"
                    disabled={!isActive || restoring === s.id}
                    onClick={async () => {
                      const ok = await ask(
                        `Restore "${s.label ?? "snapshot"}" from ${formatRelativeTime(s.createdAt)}?\n\nReplaces your current Saves + GeneratedWorlds with the snapshot's contents. Mods aren't touched.`,
                        { title: "Restore snapshot", kind: "warning" }
                      );
                      if (!ok) return;
                      setRestoring(s.id);
                      setError(null);
                      try {
                        await invoke("profile_restore_snapshot", {
                          profileId,
                          snapshotId: s.id,
                        });
                        await onRestored();
                      } catch (e) {
                        setError(typeof e === "string" ? e : `${e}`);
                      } finally {
                        setRestoring(null);
                      }
                    }}
                    className="shrink-0 inline-flex items-center px-3 py-1 rounded-md border border-[var(--color-bg-raised)] hover:border-[var(--color-accent-soft)]/40 hover:text-[var(--color-text-bright)] text-[var(--color-text-bright)]/85 text-[10px] tracking-[0.14em] uppercase disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
                  >
                    {restoring === s.id ? "Restoring…" : "Restore"}
                  </button>
                </li>
              ))}
            </ul>
          )}
          {error && (
            <div className="mt-3 rounded-md border border-[var(--color-status-danger)]/40 bg-[var(--color-status-danger)]/10 px-3 py-2 text-[11px] text-[var(--color-status-danger)] break-words">
              {error}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

// Persistent status strip at the bottom of the window. v0 is
// hard-coded — once we have a status API on packrelay.cloud the
// "All Systems Operational" pill becomes live.
function FooterStrip() {
  return (
    <footer className="shrink-0 border-t border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 backdrop-blur-sm px-6 py-2 flex items-center justify-between text-[10px] text-[var(--color-text-dim)]">
      <div className="inline-flex items-center gap-1.5">
        <span className="size-1.5 rounded-full bg-[var(--color-status-success)] shadow-[0_0_6px_rgba(80,200,120,0.7)]" />
        <span className="tracking-wide">All systems operational</span>
      </div>
      <div className="tracking-[0.14em] uppercase">
        PackRelay Launcher · v0.1
      </div>
      <div className="tracking-wide">Connected to PackRelay Network</div>
    </footer>
  );
}

/* ---------- Rail glyphs ---------- */
// Light-touch line icons sized to the 16-unit RailItem slot.
// Pure SVG, currentColor for stroke so the active/inactive theme
// flows from the parent without per-icon overrides.

function HomeGlyph() {
  return (
    <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <path d="M3 7l5-4 5 4v6a1 1 0 0 1-1 1h-2v-4H6v4H4a1 1 0 0 1-1-1V7z" strokeLinejoin="round" />
    </svg>
  );
}

function PackGlyph() {
  return (
    <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <path d="M8 1.5 2.5 4 8 6.5 13.5 4 8 1.5z" strokeLinejoin="round" />
      <path d="M2.5 4v7.5L8 14l5.5-2.5V4" strokeLinejoin="round" />
      <path d="M8 6.5V14" />
    </svg>
  );
}

function ServerGlyph() {
  return (
    <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <rect x="2.5" y="3" width="11" height="4" rx="0.8" />
      <rect x="2.5" y="9" width="11" height="4" rx="0.8" />
      <circle cx="5" cy="5" r="0.7" fill="currentColor" />
      <circle cx="5" cy="11" r="0.7" fill="currentColor" />
    </svg>
  );
}

function LibraryGlyph() {
  return (
    <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <rect x="2.5" y="2.5" width="2.5" height="11" rx="0.6" />
      <rect x="6.5" y="2.5" width="2.5" height="11" rx="0.6" />
      <path d="M11.2 3.5l2.4.6-2 9-2.4-.6 2-9z" strokeLinejoin="round" />
    </svg>
  );
}

function SettingsGlyph() {
  return (
    <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <circle cx="8" cy="8" r="2" />
      <path d="M8 1.5v2M8 12.5v2M14.5 8h-2M3.5 8h-2M12.6 3.4l-1.4 1.4M4.8 11.2l-1.4 1.4M12.6 12.6l-1.4-1.4M4.8 4.8L3.4 3.4" strokeLinecap="round" />
    </svg>
  );
}

// Stylized fox-mark glyph for the Profiles rail item — kitsune-
// adjacent visual hint that ties profile-switching to the broader
// PackRelay aesthetic.
function ProfileGlyph() {
  return (
    <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.4" aria-hidden="true">
      <path d="M3 4.5l2.5 1L4 8l1.5 4.5L8 11l2.5 1.5L12 8l-1.5-2.5L13 4.5 11 3 8 5 5 3 3 4.5z" strokeLinejoin="round" />
      <circle cx="6.5" cy="7.5" r="0.6" fill="currentColor" />
      <circle cx="9.5" cy="7.5" r="0.6" fill="currentColor" />
    </svg>
  );
}

function DiscordGlyph() {
  return (
    <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.4" aria-hidden="true">
      <path d="M4.5 4.5C5.5 4 6.7 3.7 8 3.7s2.5.3 3.5.8c1.5 2 2 4.3 1.8 6.5-.9.7-1.8 1.1-2.7 1.4l-.6-1c.4-.2.8-.4 1.2-.7-.1-.1-.2-.1-.3-.2-2 .9-4.2.9-6.2 0-.1.1-.2.1-.3.2.4.3.8.5 1.2.7l-.6 1c-.9-.3-1.8-.7-2.7-1.4-.2-2.2.3-4.5 1.8-6.5z" strokeLinejoin="round" />
      <ellipse cx="6.2" cy="8.5" rx="0.8" ry="1" fill="currentColor" />
      <ellipse cx="9.8" cy="8.5" rx="0.8" ry="1" fill="currentColor" />
    </svg>
  );
}

function NewsGlyph() {
  return (
    <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <rect x="2.5" y="3.5" width="11" height="9" rx="0.8" />
      <path d="M4.5 6h5M4.5 8h5M4.5 10h3" strokeLinecap="round" />
    </svg>
  );
}

function SupportGlyph() {
  return (
    <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
      <circle cx="8" cy="8" r="5.5" />
      <path d="M6.3 6.5a1.8 1.8 0 1 1 2.5 1.7c-.5.2-.8.6-.8 1.1V10" strokeLinecap="round" />
      <circle cx="8" cy="11.6" r="0.5" fill="currentColor" />
    </svg>
  );
}

export default App;
