import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

interface BootProps {
  children: React.ReactNode;
}

export const Boot = ({ children }: BootProps) => {
  const [ready, setReady] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    let unlistenFatal: (() => void) | undefined;
    let unlistenStartErr: (() => void) | undefined;

    const init = async () => {
      // @ts-expect-error: __TAURI_INTERNALS__ is injected by Tauri but not in standard window types
      if (!window.__TAURI_INTERNALS__) {
        setReady(true);
        return;
      }

      unlistenFatal = await listen<{
        exitCode?: number | null;
        signal?: number | null;
        lastStderrLines?: string[];
        reason?: string;
      }>("sidecar-fatal-error", (event) => {
        if (cancelled) return;
        const p = event.payload;
        const tail = (p.lastStderrLines ?? []).slice(-12).join("\n");
        setError(
          `Sidecar onverwacht gestopt (${p.reason ?? "unknown"})\nexit=${String(p.exitCode)} signal=${String(p.signal)}\n\n${tail}`
        );
      });

      unlistenStartErr = await listen<{ message?: string }>(
        "sidecar-start-error",
        (event) => {
          if (cancelled) return;
          setError(event.payload.message ?? "Sidecar kon niet starten.");
        }
      );

      try {
        console.log("Native environment detected, starting sidecar...");
        await invoke("start_agent_sidecar");
        console.log("Sidecar start invoke completed.");
        if (!cancelled) setReady(true);
      } catch (err: unknown) {
        console.error("Boot error:", err);
        if (!cancelled) {
          setError(err instanceof Error ? err.message : String(err));
        }
      }
    };

    void init();

    return () => {
      cancelled = true;
      unlistenFatal?.();
      unlistenStartErr?.();
    };
  }, []);

  if (error) {
    return (
      <div className="flex h-screen w-screen items-center justify-center bg-zinc-950 text-zinc-100">
        <div className="max-w-md space-y-4 rounded-lg border border-red-900/50 bg-red-950/20 p-8 text-center backdrop-blur-xl">
          <h1 className="text-xl font-bold text-red-400">System Boot Failure</h1>
          <div className="max-h-64 overflow-auto rounded bg-black/40 p-4 text-left">
            <pre className="whitespace-pre-wrap text-xs text-zinc-400">{error}</pre>
          </div>
          <button
            onClick={() => window.location.reload()}
            className="rounded bg-red-600 px-4 py-2 text-sm font-medium hover:bg-red-500"
          >
            Retry Boot
          </button>
        </div>
      </div>
    );
  }

  if (!ready) {
    return (
      <div className="flex h-screen w-screen flex-col items-center justify-center bg-zinc-950 text-zinc-100">
        <div className="relative h-12 w-12">
          <div className="absolute inset-0 animate-ping rounded-full bg-amber-500/20" />
          <div className="absolute inset-0 animate-pulse rounded-full border-2 border-amber-500/50 shadow-[0_0_15px_rgba(245,158,11,0.5)]" />
        </div>
        <p className="mt-8 animate-pulse text-sm font-medium tracking-widest text-zinc-400 uppercase">
          Initializing Hermes Engine
        </p>
      </div>
    );
  }

  return <>{children}</>;
};