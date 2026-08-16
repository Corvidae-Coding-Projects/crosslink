












import type { QueryClient } from "@tanstack/react-query";

const WS_URL = () => {
  const proto = window.location.protocol === "https:" ? "wss:" : "ws:";



  const token = window.sessionStorage.getItem("crosslink_api_token");
  const query = token ? `?token=${encodeURIComponent(token)}` : "";
  return `${proto}//${window.location.host}/ws${query}`;
};

interface DashboardProjectUpdated {
  type: "dashboard_project_updated";
  slug: string;
  seq: number;
}

interface DashboardAlertsChanged {
  type: "dashboard_alerts_changed";
  slug: string;
  opened: number;
  resolved: number;
  seq: number;
}

type IncomingEnvelope =
  | DashboardProjectUpdated
  | DashboardAlertsChanged
  | { type: string; seq: number };










export interface WsAlertOpenedEvent {
  slug: string;
  opened: number;
  resolved: number;
  worstSeverity?: import("./types").AlertSeverity;
}

const alertOpenListeners = new Set<(e: WsAlertOpenedEvent) => void>();




export function subscribeAlertOpens(
  cb: (e: WsAlertOpenedEvent) => void,
): () => void {
  alertOpenListeners.add(cb);
  return () => {
    alertOpenListeners.delete(cb);
  };
}



export function __emitAlertOpenForTests(event: WsAlertOpenedEvent): void {
  for (const cb of alertOpenListeners) cb(event);
}




export function connectDashboardWs(queryClient: QueryClient): () => void {
  let closed = false;
  let retryTimer: ReturnType<typeof setTimeout> | null = null;
  let socket: WebSocket | null = null;

  const open = () => {
    if (closed) return;
    socket = new WebSocket(WS_URL());
    socket.onopen = () => {



      socket?.send(JSON.stringify({ type: "subscribe", channels: ["dashboard"] }));
    };
    socket.onmessage = (ev) => {
      if (typeof ev.data !== "string") return;
      let msg: IncomingEnvelope;
      try {
        msg = JSON.parse(ev.data);
      } catch {
        return;
      }
      if (msg.type === "dashboard_project_updated") {


        const slug = (msg as DashboardProjectUpdated).slug;
        queryClient.invalidateQueries({ queryKey: ["dashboard", "projects"] });
        queryClient.invalidateQueries({ queryKey: ["dashboard", "project", slug] });
      } else if (msg.type === "dashboard_alerts_changed") {


        queryClient.invalidateQueries({ queryKey: ["dashboard", "alerts"] });
        const alertsMsg = msg as DashboardAlertsChanged;
        if (alertsMsg.opened > 0) {
          const event: WsAlertOpenedEvent = {
            slug: alertsMsg.slug,
            opened: alertsMsg.opened,
            resolved: alertsMsg.resolved,
          };
          for (const cb of alertOpenListeners) {
            try {
              cb(event);
            } catch (e) {


              console.error("alert-opens listener threw", e);
            }
          }
        }
      }
    };
    socket.onclose = () => {
      if (closed) return;


      retryTimer = setTimeout(open, 1_000);
    };
    socket.onerror = () => {
      socket?.close();
    };
  };

  open();

  return () => {
    closed = true;
    if (retryTimer) clearTimeout(retryTimer);
    socket?.close();
    socket = null;
  };
}
