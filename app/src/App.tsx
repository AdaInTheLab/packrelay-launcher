import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

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

type InstallState =
  | { kind: "idle" }
  | { kind: "running"; progress: InstallProgress | null }
  | { kind: "done"; report: InstallReport }
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
  const [packs, setPacks] = useState<CatalogPack[] | null>(null);
  const [catalogError, setCatalogError] = useState<string | null>(null);
  const [selected, setSelected] = useState<CatalogPack | null>(null);
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
        setCatalogError(typeof e === "string" ? e : `${e}`);
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

  const recordInstall = useCallback(
    (pack: CatalogPack, report: InstallReport) => {
      const entry: InstallRecord = {
        slug: pack.slug,
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
        const next = [entry, ...prev.filter((r) => r.slug !== pack.slug)];
        const trimmed = next.slice(0, HISTORY_MAX_ENTRIES);
        saveHistory(trimmed);
        return trimmed;
      });
    },
    []
  );

  const reinstallFromHistory = useCallback(
    (record: InstallRecord) => {
      // If the catalog has a matching pack, drill into InstallView
      // with it pre-selected. Otherwise the pack was removed since;
      // surface a soft error by ignoring the click.
      const match = packs?.find((p) => p.slug === record.slug);
      if (match) setSelected(match);
    },
    [packs]
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

  return (
    <div className="min-h-screen flex flex-col">
      <Header />
      <div className="flex-1 flex min-h-0">
        <HistorySidebar
          history={history}
          latestVersionBySlug={latestVersionBySlug}
          onPick={reinstallFromHistory}
          catalogReady={packs !== null}
        />
        <main className="flex-1 overflow-y-auto">
          {selected ? (
            <InstallView
              pack={selected}
              defaultDest={defaultDest}
              onBack={() => setSelected(null)}
              onInstalled={(report) => recordInstall(selected, report)}
            />
          ) : (
            <BrowseView
              packs={packs}
              error={catalogError}
              onSelect={setSelected}
            />
          )}
        </main>
      </div>
    </div>
  );
}

function Header() {
  return (
    <header className="border-b border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 backdrop-blur-sm shrink-0">
      <div className="px-6 py-4 flex items-center justify-between">
        <div className="flex items-baseline gap-3">
          <span className="font-bold tracking-tight text-base">
            PACKRELAY
          </span>
          <span className="text-[10px] tracking-[0.18em] uppercase text-[var(--color-text-dim)]">
            launcher · v0.1
          </span>
        </div>
      </div>
    </header>
  );
}

function HistorySidebar({
  history,
  latestVersionBySlug,
  onPick,
  catalogReady,
}: {
  history: InstallRecord[];
  latestVersionBySlug: Map<string, string>;
  onPick: (r: InstallRecord) => void;
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
          {history.map((r) => {
            const latest = latestVersionBySlug.get(r.slug);
            const updateAvailable =
              latest !== undefined && semverIsNewer(latest, r.version);
            return (
              <li key={`${r.slug}-${r.installedAt}`}>
                <button
                  type="button"
                  onClick={() => onPick(r)}
                  disabled={!catalogReady}
                  className="w-full text-left rounded-md px-3 py-2 mb-0.5 hover:bg-[var(--color-bg-raised)]/40 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
                >
                  <div className="flex items-center gap-1.5 mb-0.5">
                    <span className="text-xs font-medium text-[var(--color-text-bright)] truncate flex-1 min-w-0">
                      {r.name}
                    </span>
                    {updateAvailable && (
                      <span
                        title={`Update available: v${latest}`}
                        className="shrink-0 inline-flex items-center gap-0.5 px-1.5 py-0.5 rounded text-[9px] tracking-wide uppercase font-medium bg-[var(--color-accent)]/15 text-[var(--color-accent-soft)] ring-1 ring-[var(--color-accent-soft)]/40"
                      >
                        ↻ Update
                      </span>
                    )}
                  </div>
                  <div className="text-[10px] text-[var(--color-text-dim)] flex items-center justify-between gap-2">
                    <span className="font-mono truncate">
                      v{r.version}
                      {updateAvailable && (
                        <span className="text-[var(--color-accent-soft)]">
                          {" "}
                          → v{latest}
                        </span>
                      )}
                    </span>
                    <span className="shrink-0">
                      {formatRelativeTime(r.installedAt)}
                    </span>
                  </div>
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </aside>
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
                {p.coverImage ? (
                  <img
                    src={p.coverImage}
                    alt=""
                    className="w-full h-full object-cover group-hover:scale-105 transition-transform duration-500"
                  />
                ) : (
                  <div className="absolute inset-0 flex items-center justify-center text-[var(--color-text-dim)]/50 text-xs tracking-[0.2em] uppercase">
                    no cover
                  </div>
                )}
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
  onBack,
  onInstalled,
}: {
  pack: CatalogPack;
  defaultDest: string;
  onBack: () => void;
  onInstalled: (report: InstallReport) => void;
}) {
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
      const report = await invoke<InstallReport>("install_pack", {
        slug: pack.slug,
        dest,
      });
      setState({ kind: "done", report });
      onInstalled(report);
    } catch (e) {
      setState({
        kind: "error",
        message: typeof e === "string" ? e : `${e}`,
      });
    }
  }

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
        ← BROWSE
      </button>

      <div className="rounded-xl border border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)] overflow-hidden mb-6">
        <div className="aspect-[16/9] bg-[var(--color-bg-raised)] relative">
          {pack.coverImage && (
            <img
              src={pack.coverImage}
              alt=""
              className="w-full h-full object-cover"
            />
          )}
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
            disabled={running}
            className="flex-1 min-w-0 rounded-md bg-[var(--color-bg-page)] border border-[var(--color-bg-raised)] px-3 py-2 text-sm font-mono text-[var(--color-text-bright)] outline-none focus:border-[var(--color-accent-soft)]/60 focus:ring-2 focus:ring-[var(--color-accent)]/20 transition-colors disabled:opacity-60"
            placeholder="Path to your 7DTD Mods/ directory"
            spellCheck={false}
          />
          <button
            type="button"
            onClick={pickFolder}
            disabled={running}
            className="shrink-0 inline-flex items-center gap-1.5 px-3 py-2 rounded-md border border-[var(--color-bg-raised)] hover:border-[var(--color-accent-soft)]/40 hover:text-[var(--color-text-bright)] text-[var(--color-text-bright)]/85 text-xs font-medium transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
            title="Open a folder picker"
          >
            <FolderGlyph />
            Browse…
          </button>
        </div>
        <p className="mt-1.5 text-[11px] text-[var(--color-text-dim)] leading-relaxed">
          The pack folder lands inside this directory. On Windows the
          standard path is{" "}
          <code className="font-mono">%APPDATA%\7DaysToDie\Mods</code>.
        </p>

        <div className="mt-5">
          {state.kind === "idle" && (
            <button
              type="button"
              onClick={startInstall}
              disabled={!dest.trim()}
              className="inline-flex items-center gap-2 px-5 py-2.5 rounded-md bg-[var(--color-accent)] hover:bg-[var(--color-accent)]/90 disabled:opacity-50 disabled:cursor-not-allowed transition-colors text-sm font-medium"
            >
              Install pack
            </button>
          )}
          {state.kind === "running" && (
            <div>
              <div className="flex items-center justify-between text-xs mb-1.5">
                <span className="text-[var(--color-text-dim)]">
                  Installing…
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
          {state.kind === "done" && (
            <div className="rounded-md border border-[var(--color-status-success)]/40 bg-[var(--color-status-success)]/10 px-4 py-3 text-sm">
              <div className="font-medium mb-1">
                ✓ {state.report.displayName} v{state.report.version} installed
              </div>
              <div className="text-xs text-[var(--color-text-dim)]">
                {state.report.fileCount} files,{" "}
                {formatBytes(state.report.totalBytes)} →{" "}
                <code className="font-mono">{state.report.dest}</code>
              </div>
            </div>
          )}
          {state.kind === "error" && (
            <div className="rounded-md border border-[var(--color-status-danger)]/40 bg-[var(--color-status-danger)]/10 px-4 py-3 text-sm">
              <div className="font-medium mb-1">Install failed</div>
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
