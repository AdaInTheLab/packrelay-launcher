import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { ask, open as openDialog } from "@tauri-apps/plugin-dialog";

import "./App.css";

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
  attachedPack: AttachedPack | null;
};

type Tab = "packs" | "servers";

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

function App() {
  const [tab, setTab] = useState<Tab>("packs");

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
  }, []);

  const handleTabChange = useCallback((next: Tab) => {
    setTab(next);
    // Reset selections when switching tabs so the user always lands
    // on the browse view of the tab they picked.
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
        setTab("packs");
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
          onBack={() => setSelectedPack(null)}
          onInstall={() => setPackView("install")}
        />
      );
    }
  } else if (selectedServer) {
    mainContent = (
      <ServerDetailView
        server={selectedServer}
        attachedPack={
          selectedServer.attachedPack
            ? packBySlug.get(selectedServer.attachedPack.slug) ?? null
            : null
        }
        onBack={() => setSelectedServer(null)}
        onInstall={(pack) => openPackInstall(pack)}
      />
    );
  } else if (tab === "packs") {
    mainContent = (
      <BrowseView
        packs={packs}
        error={packsError}
        onSelect={openPackDetail}
      />
    );
  } else {
    mainContent = (
      <ServerBrowseView
        servers={servers}
        error={serversError}
        onSelect={setSelectedServer}
      />
    );
  }

  return (
    <div className="min-h-screen flex flex-col">
      <Header tab={tab} onTabChange={handleTabChange} />
      <div className="flex-1 flex min-h-0">
        <HistorySidebar
          history={history}
          latestVersionBySlug={latestVersionBySlug}
          onPick={reinstallFromHistory}
          onRemove={removeFromHistory}
          catalogReady={packs !== null}
        />
        <main className="flex-1 overflow-y-auto">{mainContent}</main>
      </div>
    </div>
  );
}

function Header({
  tab,
  onTabChange,
}: {
  tab: Tab;
  onTabChange: (next: Tab) => void;
}) {
  return (
    <header className="border-b border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 backdrop-blur-sm shrink-0">
      <div className="px-6 py-3 flex items-center justify-between gap-4">
        <div className="flex items-baseline gap-3">
          <span className="font-bold tracking-tight text-base">
            PACKRELAY
          </span>
          <span className="text-[10px] tracking-[0.18em] uppercase text-[var(--color-text-dim)]">
            launcher · v0.1
          </span>
        </div>
        <nav className="flex gap-1 text-[11px] tracking-[0.14em] uppercase">
          <TabButton
            active={tab === "packs"}
            onClick={() => onTabChange("packs")}
          >
            Packs
          </TabButton>
          <TabButton
            active={tab === "servers"}
            onClick={() => onTabChange("servers")}
          >
            Servers
          </TabButton>
        </nav>
      </div>
    </header>
  );
}

function TabButton({
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
      className={`px-3 py-1.5 rounded-md transition-colors ${
        active
          ? "bg-[var(--color-accent)]/15 text-[var(--color-accent-soft)] ring-1 ring-[var(--color-accent-soft)]/40"
          : "text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)] hover:bg-[var(--color-bg-raised)]/40"
      }`}
    >
      {children}
    </button>
  );
}

function HistorySidebar({
  history,
  latestVersionBySlug,
  onPick,
  onRemove,
  catalogReady,
}: {
  history: InstallRecord[];
  latestVersionBySlug: Map<string, string>;
  onPick: (r: InstallRecord) => void;
  onRemove: (r: InstallRecord) => void;
  catalogReady: boolean;
}) {
  return (
    <aside className="w-64 shrink-0 border-r border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/40 overflow-y-auto">
      <div className="px-4 py-4 border-b border-[var(--color-bg-raised)]">
        <h2 className="text-[10px] tracking-[0.18em] uppercase text-[var(--color-text-dim)]">
          Recent installs
        </h2>
      </div>
      {history.length === 0 ? (
        <div className="px-4 py-5 text-[11px] text-[var(--color-text-dim)] leading-relaxed">
          Packs you install land here. Click an entry later to jump
          back to its page.
        </div>
      ) : (
        <ul className="px-2 py-2">
          {history.map((r) => (
            <HistoryRow
              key={`${r.slug}-${r.installedAt}`}
              record={r}
              latestVersionBySlug={latestVersionBySlug}
              onPick={onPick}
              onRemove={onRemove}
              catalogReady={catalogReady}
            />
          ))}
        </ul>
      )}
    </aside>
  );
}

// One row in the install-history list. Holds its own
// verify/repair state machine so multiple rows can be in different
// states at once (e.g. one verifying while another is showing a
// repaired badge). Click-the-row still reinstalls; the Verify
// button stops propagation so the two actions don't collide.
function HistoryRow({
  record,
  latestVersionBySlug,
  onPick,
  onRemove,
  catalogReady,
}: {
  record: InstallRecord;
  latestVersionBySlug: Map<string, string>;
  onPick: (r: InstallRecord) => void;
  onRemove: (r: InstallRecord) => void;
  catalogReady: boolean;
}) {
  const [verify, setVerify] = useState<RowVerifyState>({ kind: "idle" });
  const [uninstall, setUninstall] = useState<RowUninstallState>({ kind: "idle" });

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

  // Uninstall flow: native confirm → uninstall_pack → either drop
  // from history (clean run) or show inline partial-failure panel
  // listing the locked files so the user can act on them.
  const runUninstall = useCallback(async () => {
    const ok = await ask(
      `Remove ${record.name} v${record.version}?\n\nDeletes the pack's files under ${record.dest}. The Mods/ folder and any other packs inside it are left alone.`,
      { title: "Uninstall pack", kind: "warning" }
    );
    if (!ok) return;
    setUninstall({ kind: "uninstalling" });
    // Clear the verify panel — its state is about to be obsolete
    // whether the uninstall succeeds or fails.
    setVerify({ kind: "idle" });
    try {
      const report = await invoke<UninstallReport>("uninstall_pack", {
        dest: record.dest,
      });
      if (report.filesFailed.length === 0) {
        // Clean run — drop from history; the row unmounts.
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
    <li>
      <button
        type="button"
        onClick={() => onPick(record)}
        disabled={!catalogReady}
        className="w-full text-left rounded-md px-3 py-2 hover:bg-[var(--color-bg-raised)]/40 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
      >
        <div className="flex items-center gap-1.5 mb-0.5">
          <span className="text-xs font-medium text-[var(--color-text-bright)] truncate flex-1 min-w-0">
            {record.name}
          </span>
          {updateAvailable && (
            <span
              title={`Update available: v${latest}`}
              className="shrink-0 inline-flex items-center gap-0.5 px-1.5 py-0.5 rounded text-[9px] tracking-wide uppercase font-medium bg-[var(--color-accent)]/15 text-[var(--color-accent-soft)] ring-1 ring-[var(--color-accent-soft)]/40"
            >
              ↻ Update
            </span>
          )}
          {/* Verify is a secondary action on the row — small, low-
              contrast, and stops propagation so the surrounding
              re-install button doesn't also fire. */}
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
            className="shrink-0 inline-flex items-center px-1.5 py-0.5 rounded text-[10px] uppercase tracking-wide text-[var(--color-text-dim)] hover:text-[var(--color-accent-soft)] hover:bg-[var(--color-bg-raised)]/60 cursor-pointer transition-colors"
          >
            {verify.kind === "verifying" || verify.kind === "repairing"
              ? "…"
              : "✓"}
          </span>
          {/* Uninstall is the destructive sibling — same low-
              contrast styling, hover-only red so it doesn't read as
              dangerous at rest. Native confirm gates the actual
              delete. */}
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
            className="shrink-0 inline-flex items-center px-1.5 py-0.5 rounded text-[10px] uppercase tracking-wide text-[var(--color-text-dim)] hover:text-[var(--color-status-danger)] hover:bg-[var(--color-bg-raised)]/60 cursor-pointer transition-colors"
          >
            {uninstall.kind === "uninstalling" ? "…" : "✕"}
          </span>
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
          <span className="shrink-0">{formatRelativeTime(record.installedAt)}</span>
        </div>
      </button>
      <VerifyStatus state={verify} onRepair={runRepair} onDismiss={() => setVerify({ kind: "idle" })} />
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
}: {
  packs: CatalogPack[] | null;
  error: string | null;
  onSelect: (p: CatalogPack) => void;
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

  return (
    <div className="px-6 py-8">
      <div className="mb-6">
        <h1 className="text-xl font-semibold tracking-tight">Browse packs</h1>
        <p className="text-xs text-[var(--color-text-dim)] mt-1">
          Click a pack to install it into your 7DTD Mods/ directory.
        </p>
      </div>
      <ul className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4">
        {packs.map((p) => (
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
                <div className="mt-3 flex items-center justify-between text-[10px] text-[var(--color-text-dim)]">
                  <span>{p.fileCount.toLocaleString()} files</span>
                  <span>{formatBytes(p.totalSizeBytes)}</span>
                </div>
              </div>
            </button>
          </li>
        ))}
      </ul>
    </div>
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
function DetailView({
  pack,
  installedRecord,
  onBack,
  onInstall,
}: {
  pack: CatalogPack;
  installedRecord: InstallRecord | null;
  onBack: () => void;
  onInstall: () => void;
}) {
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
      <button
        type="button"
        onClick={onBack}
        className="text-[10px] tracking-[0.18em] uppercase text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)] mb-4"
      >
        ← BROWSE
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
}: {
  servers: CatalogServer[] | null;
  error: string | null;
  onSelect: (s: CatalogServer) => void;
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

  return <ServerBrowseLoaded servers={servers} onSelect={onSelect} />;
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
}: {
  servers: CatalogServer[];
  onSelect: (s: CatalogServer) => void;
}) {
  // Filter state is local to this component — it resets if the
  // user leaves the Servers tab and comes back, which is fine for
  // v1. Hoist to App if cross-navigation persistence becomes a need.
  const [region, setRegion] = useState<string>("");
  const [onlineOnly, setOnlineOnly] = useState(false);
  const [notFull, setNotFull] = useState(false);

  const filtered = servers.filter((s) => {
    if (region && s.region !== region) return false;
    if (onlineOnly && !s.online) return false;
    if (notFull && s.currentPlayers >= s.maxPlayers) return false;
    return true;
  });
  const hasFilters = !!region || onlineOnly || notFull;
  const clearFilters = () => {
    setRegion("");
    setOnlineOnly(false);
    setNotFull(false);
  };

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
          onClick={() => setOnlineOnly((v) => !v)}
          dotColor="success"
        >
          Online only
        </FilterChip>
        <FilterChip
          active={notFull}
          onClick={() => setNotFull((v) => !v)}
          dotColor="accent"
        >
          Not full
        </FilterChip>
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
        <div className="font-medium text-[var(--color-text-bright)] truncate mb-1">
          {s.name}
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
  onBack,
  onInstall,
}: {
  server: CatalogServer;
  /** Full CatalogPack for the attached pack — null if the server
   *  has no pack OR the catalog hasn't loaded yet (we can't install
   *  what we don't have full metadata for). */
  attachedPack: CatalogPack | null;
  onBack: () => void;
  onInstall: (pack: CatalogPack) => void;
}) {
  const cover = server.attachedPack?.coverImage;
  return (
    <div className="px-6 py-8 max-w-2xl mx-auto">
      <button
        type="button"
        onClick={onBack}
        className="text-[10px] tracking-[0.18em] uppercase text-[var(--color-text-dim)] hover:text-[var(--color-text-bright)] mb-4"
      >
        ← SERVERS
      </button>

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
              Install the pack
            </div>
            <div className="flex items-start gap-3 mb-4">
              <div className="size-14 rounded-md bg-[var(--color-bg-raised)] shrink-0 relative overflow-hidden">
                <CoverImage
                  src={attachedPack.coverImage}
                  className="absolute inset-0 w-full h-full object-cover"
                />
              </div>
              <div className="min-w-0 flex-1">
                <div className="text-sm font-medium text-[var(--color-text-bright)] truncate">
                  {attachedPack.name}
                  {attachedPack.latestVersion && (
                    <span className="font-mono text-[11px] text-[var(--color-text-dim)] ml-1.5">
                      v{attachedPack.latestVersion}
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
            <button
              type="button"
              onClick={() => onInstall(attachedPack)}
              className="w-full inline-flex items-center justify-center gap-2 px-5 py-2.5 rounded-md bg-[var(--color-accent)] hover:bg-[var(--color-accent)]/90 transition-colors text-sm font-medium"
            >
              Install pack & get connect address
            </button>
            <p className="mt-2 text-[11px] text-[var(--color-text-dim)] leading-relaxed">
              We&apos;ll lay down the pack files, then surface the
              direct-connect address so you can paste it into the
              7DTD launcher.
            </p>
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
      await invoke("launch_game");
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
          Steam is launching 7DTD. The address is on your clipboard
          — paste it into <span className="text-[var(--color-text-bright)]">Join a Game → Connect to IP</span>.
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
          Opens 7DTD via Steam and copies the address. Paste it into
          Join a Game → Connect to IP once the menu loads.
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

export default App;
