/**
 * Bottom-right toast announcing an available update.
 *
 * Four states map to four renderings:
 *   - idle       → null (no toast)
 *   - available  → "Update vX.Y.Z available" + Install / Later
 *   - downloading → progress bar + cancel-not-allowed
 *   - installed  → "Restarting…" pill (auto-relaunch fires from
 *                  the hook a beat after this lands)
 *   - error      → "Update failed: <msg>" + Dismiss
 *
 * Sticks to a non-blocking dock so the user can keep using the
 * launcher while an update lands. Their next launch picks up the
 * new version even if they dismiss now.
 */

import type { useAutoUpdate } from "./useAutoUpdate";

type UpdaterState = ReturnType<typeof useAutoUpdate>;

export function UpdateToast({
  phase,
  download,
  dismiss,
}: UpdaterState) {
  if (phase.kind === "idle") return null;

  return (
    <div className="fixed bottom-4 right-4 z-50 w-80 max-w-[90vw] rounded-lg border border-white/10 bg-zinc-900/95 backdrop-blur-sm shadow-2xl text-sm">
      {phase.kind === "available" && (
        <AvailableCard
          version={phase.version}
          body={phase.body}
          download={download}
          dismiss={dismiss}
        />
      )}
      {phase.kind === "downloading" && (
        <DownloadingCard
          version={phase.version}
          downloaded={phase.downloaded}
          total={phase.total}
        />
      )}
      {phase.kind === "installed" && (
        <InstalledCard version={phase.version} />
      )}
      {phase.kind === "error" && (
        <ErrorCard message={phase.message} dismiss={dismiss} />
      )}
    </div>
  );
}

function AvailableCard({
  version,
  body,
  download,
  dismiss,
}: {
  version: string;
  body: string | null;
  download: () => void;
  dismiss: () => void;
}) {
  return (
    <div className="p-3">
      <div className="flex items-baseline gap-2 mb-1">
        <span aria-hidden="true">✨</span>
        <div className="text-zinc-100 font-medium">
          Update v{version} available
        </div>
      </div>
      {body && (
        <div className="text-[12px] text-zinc-400 leading-snug mb-2 line-clamp-3 whitespace-pre-wrap">
          {body.trim()}
        </div>
      )}
      <div className="flex gap-2 mt-2">
        <button
          type="button"
          onClick={download}
          className="px-3 py-1.5 rounded-md bg-violet-600 hover:bg-violet-500 text-white text-xs font-medium transition-colors"
        >
          Install
        </button>
        <button
          type="button"
          onClick={dismiss}
          className="px-3 py-1.5 rounded-md border border-white/10 text-zinc-300 hover:text-zinc-100 hover:border-white/20 text-xs transition-colors"
        >
          Later
        </button>
      </div>
    </div>
  );
}

function DownloadingCard({
  version,
  downloaded,
  total,
}: {
  version: string;
  downloaded: number;
  total: number | null;
}) {
  const pct =
    total && total > 0
      ? Math.min(100, Math.round((downloaded / total) * 100))
      : null;
  return (
    <div className="p-3">
      <div className="flex items-baseline gap-2 mb-2">
        <span aria-hidden="true">⇣</span>
        <div className="text-zinc-100 font-medium">
          Downloading v{version}
          {pct !== null && (
            <span className="ml-2 text-zinc-400 font-normal">
              {pct}%
            </span>
          )}
        </div>
      </div>
      <div className="h-1 rounded-full bg-white/5 overflow-hidden">
        <div
          className="h-full bg-violet-500 transition-all"
          style={{
            width: pct !== null ? `${pct}%` : "30%",
            // When we don't know the total, animate a steady
            // indeterminate-style fill so the bar still feels
            // alive.
            animation: pct === null ? "pulse 1.5s ease-in-out infinite" : undefined,
          }}
        />
      </div>
    </div>
  );
}

function InstalledCard({ version }: { version: string }) {
  return (
    <div className="p-3 flex items-baseline gap-2">
      <span aria-hidden="true">✓</span>
      <div className="text-zinc-100">
        v{version} installed —{" "}
        <span className="text-zinc-400">restarting…</span>
      </div>
    </div>
  );
}

function ErrorCard({
  message,
  dismiss,
}: {
  message: string;
  dismiss: () => void;
}) {
  return (
    <div className="p-3">
      <div className="flex items-baseline gap-2 mb-1">
        <span aria-hidden="true">⚠</span>
        <div className="text-zinc-100 font-medium">Update failed</div>
      </div>
      <div className="text-[12px] text-zinc-400 leading-snug mb-2 line-clamp-3">
        {message}
      </div>
      <button
        type="button"
        onClick={dismiss}
        className="px-3 py-1.5 rounded-md border border-white/10 text-zinc-300 hover:text-zinc-100 hover:border-white/20 text-xs transition-colors"
      >
        Dismiss
      </button>
    </div>
  );
}
