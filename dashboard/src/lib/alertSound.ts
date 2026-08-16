













import type { AlertSeverity } from "@/api/types";

import { getPreferences, subscribePreferences } from "./preferences";
import { subscribeAlertOpens } from "@/api/ws";


let ctx: AudioContext | null = null;

function audioContext(): AudioContext | null {
  if (typeof window === "undefined") return null;
  if (ctx) return ctx;
  const AC =
    window.AudioContext ??
    (window as unknown as { webkitAudioContext?: typeof AudioContext })
      .webkitAudioContext;
  if (!AC) return null;
  try {
    ctx = new AC();
    return ctx;
  } catch {
    return null;
  }
}




const TONE_HZ: Record<AlertSeverity, number> = {
  info: 440,
  warning: 587,
  critical: 784,
};



export function playToneFor(severity: AlertSeverity): boolean {
  const ac = audioContext();
  if (!ac) return false;




  if (ac.state !== "running") {
    void ac.resume().catch(() => undefined);
    return false;
  }
  try {
    const osc = ac.createOscillator();
    const gain = ac.createGain();
    osc.type = "sine";
    osc.frequency.value = TONE_HZ[severity];

    const now = ac.currentTime;
    gain.gain.setValueAtTime(0, now);
    gain.gain.linearRampToValueAtTime(0.15, now + 0.02);
    gain.gain.exponentialRampToValueAtTime(0.001, now + 0.25);
    osc.connect(gain).connect(ac.destination);
    osc.start(now);
    osc.stop(now + 0.27);
    return true;
  } catch {
    return false;
  }
}





export interface AlertOpenedEvent {
  slug: string;
  opened: number;


  worstSeverity?: AlertSeverity;
}




export function installAlertSoundBridge(): () => void {


  const unsubWs = subscribeAlertOpens((event) => {
    const prefs = getPreferences();
    if (!prefs.audibleEnabled) return;
    if (event.opened <= 0) return;
    const severity = event.worstSeverity ?? "critical";
    if (!prefs.audibleSeverities.includes(severity)) return;
    playToneFor(severity);
  });






  const unsubPref = subscribePreferences(() => undefined);

  return () => {
    unsubWs();
    unsubPref();
  };
}
