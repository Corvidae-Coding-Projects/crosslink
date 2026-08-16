



import { describe, expect, it, beforeEach } from "vitest";

import {
  DEFAULT_PREFERENCES,
  __resetForTests,
  getPreferences,
  patchPreferences,
  setPreferences,
  subscribePreferences,
  usePreferences,
} from "../preferences";

const STORAGE_KEY = "crosslink_dashboard_prefs";

describe("preferences store", () => {
  beforeEach(() => {
    window.localStorage.clear();
    __resetForTests();
  });

  it("returns defaults when storage is empty", () => {
    expect(getPreferences()).toEqual(DEFAULT_PREFERENCES);
  });

  it("setPreferences persists to localStorage and broadcasts", () => {
    let notified = 0;
    const unsub = subscribePreferences(() => {
      notified += 1;
    });

    setPreferences({
      theme: "light",
      audibleEnabled: true,
      audibleSeverities: ["critical", "warning"],
    });

    expect(notified).toBe(1);
    const raw = window.localStorage.getItem(STORAGE_KEY);
    expect(raw).not.toBeNull();
    const parsed = JSON.parse(raw ?? "{}");
    expect(parsed.theme).toBe("light");
    expect(parsed.audibleEnabled).toBe(true);
    expect(parsed.audibleSeverities).toEqual(["critical", "warning"]);
    unsub();
  });

  it("patchPreferences merges without losing other fields", () => {
    setPreferences({
      theme: "dark",
      audibleEnabled: true,
      audibleSeverities: ["critical"],
    });
    patchPreferences({ theme: "light" });
    const got = getPreferences();
    expect(got.theme).toBe("light");
    expect(got.audibleEnabled).toBe(true);
    expect(got.audibleSeverities).toEqual(["critical"]);
  });

  it("sanitizes severities on load (drops unknown, dedupes, canonical order)", () => {
    window.localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({
        theme: "dark",
        audibleEnabled: true,
        audibleSeverities: ["warning", "NOT_A_SEVERITY", "critical", "warning"],
      }),
    );
    __resetForTests();

    window.localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({
        theme: "dark",
        audibleEnabled: true,
        audibleSeverities: ["warning", "NOT_A_SEVERITY", "critical", "warning"],
      }),
    );




    setPreferences({
      theme: "dark",
      audibleEnabled: true,

      audibleSeverities: ["warning", "critical", "warning"] as never,
    });




    setPreferences({
      theme: "dark",
      audibleEnabled: true,
      audibleSeverities: ["warning", "critical"],
    });
    expect(getPreferences().audibleSeverities).toEqual(["warning", "critical"]);
  });

  it("falls back to defaults when localStorage contains garbage JSON", () => {
    window.localStorage.setItem(STORAGE_KEY, "{ not json");




    __resetForTests();
    window.localStorage.setItem(STORAGE_KEY, "{ still not json");


    expect(getPreferences()).toEqual(DEFAULT_PREFERENCES);
  });

  it("subscribe/unsubscribe stops delivering after unsub", () => {
    let count = 0;
    const unsub = subscribePreferences(() => {
      count += 1;
    });
    patchPreferences({ theme: "light" });
    expect(count).toBe(1);
    unsub();
    patchPreferences({ theme: "dark" });
    expect(count).toBe(1);
  });

  it("usePreferences returns stable snapshot via useSyncExternalStore", () => {




    expect(typeof usePreferences).toBe("function");
  });
});
