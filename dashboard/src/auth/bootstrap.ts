




export const TOKEN_STORAGE_KEY = "crosslink_api_token";

















export function bootstrapAuth(): string | null {
  let token: string | null = null;

  try {
    const url = new URL(window.location.href);
    const urlToken = url.searchParams.get("token");
    if (urlToken) {
      token = urlToken;
      try {
        window.sessionStorage.setItem(TOKEN_STORAGE_KEY, urlToken);
      } catch (error) {
        void error;
      }
      url.searchParams.delete("token");
      window.history.replaceState({}, "", url.toString());
    } else {
      try {
        token = window.sessionStorage.getItem(TOKEN_STORAGE_KEY);
      } catch {
        token = null;
      }
    }
  } catch {

    return null;
  }

  if (!token) return null;

  const nativeFetch = globalThis.fetch.bind(globalThis);
  const capturedToken = token;
  globalThis.fetch = ((input, init) => {
    const headers = new Headers(init?.headers);
    headers.set("Authorization", `Bearer ${capturedToken}`);
    return nativeFetch(input, { ...init, headers });
  }) as typeof globalThis.fetch;

  return token;
}
