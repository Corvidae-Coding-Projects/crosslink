



import type { ProjectListItem } from "@/api/types";

export type TileSeverity = "nominal" | "warning" | "critical" | "paused" | "unreachable";





export function tileSeverity(item: ProjectListItem): TileSeverity {
  if (item.status === "paused") return "paused";
  if (item.status === "error") return "unreachable";
  const c = item.counters;
  if (c.stale_locks > 0) return "critical";
  if (c.overdue_issues > 0 || c.blocked_issues > 0 || c.ci_status === "failing") {
    return "warning";
  }
  return "nominal";
}
