



import { useState } from "react";

import { downloadAuthenticated } from "@/lib/download";

interface ExportMenuProps {

  label: string;


  pathPrefix: string;


  filenameStem: string;
}

export function ExportMenu({ label, pathPrefix, filenameStem }: ExportMenuProps) {
  const [busy, setBusy] = useState<"csv" | "json" | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function run(format: "csv" | "json") {
    setBusy(format);
    setError(null);
    try {
      await downloadAuthenticated({
        path: `${pathPrefix}.${format}`,
        filename: `${filenameStem}.${format}`,
      });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  }

  return (
    <span className="flex items-center gap-1 text-xs text-muted-foreground">
      <span className="hidden sm:inline">export {label}:</span>
      <button
        type="button"
        onClick={() => run("csv")}
        disabled={busy !== null}
        aria-label={`Download ${label} as CSV`}
        className="rounded border px-2 py-0.5 hover:bg-accent/10 disabled:opacity-50"
      >
        {busy === "csv" ? "…" : "CSV"}
      </button>
      <button
        type="button"
        onClick={() => run("json")}
        disabled={busy !== null}
        aria-label={`Download ${label} as JSON`}
        className="rounded border px-2 py-0.5 hover:bg-accent/10 disabled:opacity-50"
      >
        {busy === "json" ? "…" : "JSON"}
      </button>
      {error && <span className="text-rose-400">{error}</span>}
    </span>
  );
}
