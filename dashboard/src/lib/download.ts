











const API_BASE = "/api/v1/dashboard";

export interface DownloadOptions {

  path: string;


  filename: string;
}



export async function downloadAuthenticated(opts: DownloadOptions): Promise<void> {
  const resp = await fetch(`${API_BASE}${opts.path}`, {
    method: "GET",
    headers: { Accept: "*/*" },
  });
  if (!resp.ok) {
    let message = `HTTP ${resp.status}`;
    try {
      const body = (await resp.json()) as { error?: string };
      if (body.error) message = body.error;
    } catch (error) {
      void error;
    }
    throw new Error(message);
  }
  const blob = await resp.blob();
  const url = URL.createObjectURL(blob);
  try {
    const a = document.createElement("a");
    a.href = url;
    a.download = opts.filename;
    document.body.appendChild(a);
    a.click();
    a.remove();
  } finally {



    setTimeout(() => URL.revokeObjectURL(url), 1_000);
  }
}
