/**
 * Auto-update hook for the launcher.
 *
 * Calls tauri-plugin-updater's `check()` once at mount, then
 * exposes:
 *   - the available update's version + body (release notes), or null
 *   - a `download` function that streams the install + relaunches
 *   - a `dismiss` function (per-version, so the prompt re-appears
 *     for the *next* release)
 *
 * Design choices:
 *   - We never block the UI on the check — failures are silent.
 *     A broken update endpoint shouldn't keep someone from
 *     launching their game.
 *   - The dismiss state is keyed on `version` and lives in
 *     localStorage so it survives reloads. When a newer version
 *     ships, the user sees the prompt again.
 *   - In dev (when window.__TAURI_INTERNALS__ is undefined, e.g.
 *     `vite dev` outside the wrapper), we no-op so the hook
 *     compiles without throwing.
 */

import { useEffect, useState } from "react";

type Phase =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "upToDate"; checkedAt: string }
  | { kind: "available"; version: string; body: string | null }
  | {
      kind: "downloading";
      version: string;
      downloaded: number;
      total: number | null;
    }
  | { kind: "installed"; version: string }
  | { kind: "error"; message: string };

const DISMISS_KEY = "packrelay-update-dismissed-version";

function isInTauri(): boolean {
  // The runtime injects this on window when running inside the
  // Tauri shell. Outside (vite dev), the plugin import would still
  // succeed but `check()` would throw on first call.
  return (
    typeof window !== "undefined" &&
    (window as unknown as { __TAURI_INTERNALS__?: unknown })
      .__TAURI_INTERNALS__ !== undefined
  );
}

function readDismissed(): string | null {
  try {
    return localStorage.getItem(DISMISS_KEY);
  } catch {
    return null;
  }
}

function writeDismissed(version: string) {
  try {
    localStorage.setItem(DISMISS_KEY, version);
  } catch {
    // Private-mode / quota — skip the persistence, the in-memory
    // dismiss still works for the current session.
  }
}

export function useAutoUpdate() {
  const [phase, setPhase] = useState<Phase>({ kind: "idle" });

  useEffect(() => {
    if (!isInTauri()) return;
    let cancelled = false;
    (async () => {
      try {
        const { check } = await import("@tauri-apps/plugin-updater");
        const update = await check();
        if (cancelled || !update) return;
        // Respect a prior dismiss for *this exact version*. Bumping
        // the release version resets it.
        if (readDismissed() === update.version) return;
        setPhase({
          kind: "available",
          version: update.version,
          body: update.body ?? null,
        });
      } catch (err) {
        // Network failures, signature mismatch, malformed manifest —
        // all silent. The user still launches normally.
        console.warn("[updater] check failed:", err);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  async function download() {
    if (phase.kind !== "available") return;
    const version = phase.version;
    setPhase({
      kind: "downloading",
      version,
      downloaded: 0,
      total: null,
    });
    try {
      const { check } = await import("@tauri-apps/plugin-updater");
      // Re-check so we have the live Update handle. The earlier
      // one is stale because hooks don't survive between effects
      // in a way that lets us keep the Rust-side object alive.
      const update = await check();
      if (!update) {
        setPhase({ kind: "error", message: "Update is no longer available." });
        return;
      }

      let downloaded = 0;
      let total: number | null = null;
      await update.downloadAndInstall((event) => {
        if (event.event === "Started") {
          total = event.data.contentLength ?? null;
        } else if (event.event === "Progress") {
          downloaded += event.data.chunkLength;
          setPhase({ kind: "downloading", version, downloaded, total });
        }
      });

      setPhase({ kind: "installed", version });

      // Auto-relaunch a beat later so the user sees the "installed"
      // state briefly. Failure to relaunch is recoverable — the
      // user just opens the app manually after the next launch.
      try {
        const { relaunch } = await import("@tauri-apps/plugin-process");
        setTimeout(() => relaunch(), 1200);
      } catch (err) {
        console.warn("[updater] relaunch failed:", err);
      }
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setPhase({ kind: "error", message });
    }
  }

  function dismiss() {
    if (phase.kind === "available") writeDismissed(phase.version);
    setPhase({ kind: "idle" });
  }

  /**
   * Manually re-run the updater check. Used by the Settings view's
   * "Check for updates" button. Unlike the mount-effect check, this:
   *
   *   - Sets phase to "checking" while it runs (so the UI can spin)
   *   - Clears any prior per-version dismiss (the user is actively
   *     asking, so respect that over their earlier "Later" click)
   *   - Settles to "upToDate" with a timestamp when there's nothing,
   *     so the Settings UI can render "You're on the latest version"
   *   - Or to "available" when there is something
   *   - Or to "error" on any failure (instead of silent, since the
   *     user just clicked a button expecting a response)
   *
   * No-ops outside the Tauri runtime (Vite-only dev).
   */
  async function checkNow() {
    if (!isInTauri()) {
      // In Vite-only dev there's no updater plugin available. Lie
      // gracefully so the Settings UI can demo without exploding.
      setPhase({ kind: "upToDate", checkedAt: new Date().toISOString() });
      return;
    }
    setPhase({ kind: "checking" });
    try {
      const { check } = await import("@tauri-apps/plugin-updater");
      const update = await check();
      if (!update) {
        setPhase({
          kind: "upToDate",
          checkedAt: new Date().toISOString(),
        });
        return;
      }
      // User clicked Check Now -- forget any prior dismiss for this
      // version. They're opting in.
      try {
        localStorage.removeItem(DISMISS_KEY);
      } catch {
        // Storage disabled -- next mount-check will respect a stale
        // dismiss; fine for this rare case.
      }
      setPhase({
        kind: "available",
        version: update.version,
        body: update.body ?? null,
      });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setPhase({ kind: "error", message });
    }
  }

  return { phase, download, dismiss, checkNow };
}
