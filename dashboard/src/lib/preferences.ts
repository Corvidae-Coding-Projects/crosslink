











import { useSyncExternalStore } from "react";

import type { AlertSeverity } from "@/api/types";

const STORAGE_KEY = "crosslink_dashboard_prefs";

export type ThemePreference = "light" | "dark" | "system";

export interface Preferences {
  theme: ThemePreference;
  audibleEnabled: boolean;


  audibleSeverities: AlertSeverity[];
}

export const DEFAULT_PREFERENCES: Preferences = {
  theme: "system",
  audibleEnabled: false,
  audibleSeverities: ["critical"],
};

function readStorage(): Preferences {
  if (typeof window === "undefined") return DEFAULT_PREFERENCES;
  let raw: string | null = null;
  try {
    raw = window.localStorage.getItem(STORAGE_KEY);
  } catch {
    return DEFAULT_PREFERENCES;
  }
  if (!raw) return DEFAULT_PREFERENCES;
  try {
    const parsed = JSON.parse(raw) as Partial<Preferences>;
    return {
      theme: isThemePreference(parsed.theme) ? parsed.theme : DEFAULT_PREFERENCES.theme,
      audibleEnabled:
        typeof parsed.audibleEnabled === "boolean"
          ? parsed.audibleEnabled
          : DEFAULT_PREFERENCES.audibleEnabled,
      audibleSeverities: sanitizeSeverities(parsed.audibleSeverities),
    };
  } catch {
    return DEFAULT_PREFERENCES;
  }
}

function isThemePreference(v: unknown): v is ThemePreference {
  return v === "light" || v === "dark" || v === "system";
}

function sanitizeSeverities(v: unknown): AlertSeverity[] {
  if (!Array.isArray(v)) return DEFAULT_PREFERENCES.audibleSeverities;
  const allowed: AlertSeverity[] = ["info", "warning", "critical"];
  const seen = new Set<AlertSeverity>();
  for (const item of v) {
    if (allowed.includes(item as AlertSeverity)) {
      seen.add(item as AlertSeverity);
    }
  }
  return allowed.filter((s) => seen.has(s));
}

let current: Preferences = readStorage();
const subscribers = new Set<() => void>();

function writeStorage(next: Preferences): void {
  if (typeof window === "undefined") return;
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(next));
  } catch (error) {
    void error;
  }
}


export function setPreferences(next: Preferences): void {
  current = {
    theme: next.theme,
    audibleEnabled: next.audibleEnabled,

    audibleSeverities: [...next.audibleSeverities],
  };
  writeStorage(current);
  for (const cb of subscribers) cb();
}


export function patchPreferences(patch: Partial<Preferences>): void {
  setPreferences({ ...current, ...patch });
}


export function getPreferences(): Preferences {
  return current;
}


export function subscribePreferences(cb: () => void): () => void {
  subscribers.add(cb);
  return () => {
    subscribers.delete(cb);
  };
}



export function usePreferences(): Preferences {
  return useSyncExternalStore(subscribePreferences, getPreferences, getPreferences);
}



export function __resetForTests(): void {
  current = { ...DEFAULT_PREFERENCES, audibleSeverities: [...DEFAULT_PREFERENCES.audibleSeverities] };
  subscribers.clear();
  if (typeof window !== "undefined") {
    try {
      window.localStorage.removeItem(STORAGE_KEY);
    } catch (error) {
      void error;
    }
  }
}
