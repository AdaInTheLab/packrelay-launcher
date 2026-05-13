import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

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

const PACKRELAY_DEFAULT_DEST =
  navigator.platform.toLowerCase().includes("win")
    ? "C:\\7DaysToDie\\Mods"
    : "/opt/7DaysToDie/Mods";

function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
}

function App() {
  const [packs, setPacks] = useState<CatalogPack[] | null>(null);
  const [catalogError, setCatalogError] = useState<string | null>(null);
  const [selected, setSelected] = useState<CatalogPack | null>(null);

  useEffect(() => {
    (async () => {
      try {
        const list = await invoke<CatalogPack[]>("list_packs");
        setPacks(list);
      } catch (e) {
        setCatalogError(typeof e === "string" ? e : `${e}`);
      }
    })();
  }, []);

  return (
    <div className="min-h-screen flex flex-col">
      <Header />
      <main className="flex-1 overflow-y-auto">
        {selected ? (
          <InstallView pack={selected} onBack={() => setSelected(null)} />
        ) : (
          <BrowseView
            packs={packs}
            error={catalogError}
            onSelect={setSelected}
          />
        )}
      </main>
    </div>
  );
}

function Header() {
  return (
    <header className="border-b border-[var(--color-bg-raised)] bg-[var(--color-bg-panel)]/60 backdrop-blur-sm">
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
  onBack,
}: {
  pack: CatalogPack;
  onBack: () => void;
}) {
  const [dest, setDest] = useState(PACKRELAY_DEFAULT_DEST);
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

  async function startInstall() {
    setState({ kind: "running", progress: null });
    try {
      const report = await invoke<InstallReport>("install_pack", {
        slug: pack.slug,
        dest,
      });
      setState({ kind: "done", report });
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
        <input
          id="dest"
          type="text"
          value={dest}
          onChange={(e) => setDest(e.currentTarget.value)}
          disabled={running}
          className="w-full rounded-md bg-[var(--color-bg-page)] border border-[var(--color-bg-raised)] px-3 py-2 text-sm font-mono text-[var(--color-text-bright)] outline-none focus:border-[var(--color-accent-soft)]/60 focus:ring-2 focus:ring-[var(--color-accent)]/20 transition-colors disabled:opacity-60"
          placeholder="Path to your 7DTD Mods/ directory"
          spellCheck={false}
        />
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

export default App;
