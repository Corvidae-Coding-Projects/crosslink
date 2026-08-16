








import { useEffect } from "react";

import {
  subscribePreferences,
  getPreferences,
  type ThemePreference,
} from "./preferences";

const LIGHT_CLASS = "theme-light";



function applyResolved(resolved: "light" | "dark"): void {
  if (typeof document === "undefined") return;
  const el = document.documentElement;
  if (resolved === "light") el.classList.add(LIGHT_CLASS);
  else el.classList.remove(LIGHT_CLASS);
}

function prefersDark(): boolean {
  if (typeof window === "undefined" || !window.matchMedia) return true;
  return window.matchMedia("(prefers-color-scheme: dark)").matches;
}


export function resolveTheme(pref: ThemePreference): "light" | "dark" {
  if (pref === "system") return prefersDark() ? "dark" : "light";
  return pref;
}




export function installThemeObserver(): () => void {
  let disposed = false;
  let mediaListener: ((e: MediaQueryListEvent) => void) | null = null;
  let mediaQuery: MediaQueryList | null = null;

  const apply = () => {
    if (disposed) return;
    const prefs = getPreferences();
    applyResolved(resolveTheme(prefs.theme));




    const wantListener = prefs.theme === "system";
    if (wantListener && !mediaListener && typeof window !== "undefined" && window.matchMedia) {
      mediaQuery = window.matchMedia("(prefers-color-scheme: dark)");
      mediaListener = () => {
        if (getPreferences().theme === "system") {
          applyResolved(prefersDark() ? "dark" : "light");
        }
      };



      mediaQuery.addEventListener("change", mediaListener);
    } else if (!wantListener && mediaListener && mediaQuery) {
      mediaQuery.removeEventListener("change", mediaListener);
      mediaListener = null;
      mediaQuery = null;
    }
  };

  apply();
  const unsub = subscribePreferences(apply);
  return () => {
    disposed = true;
    unsub();
    if (mediaListener && mediaQuery) {
      mediaQuery.removeEventListener("change", mediaListener);
    }
  };
}



export function useThemeBridge(): void {
  useEffect(() => installThemeObserver(), []);
}
