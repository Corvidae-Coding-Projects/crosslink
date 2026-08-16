



import type { AlertItem, AlertSeverity } from "@/api/types";

export const SEVERITY_ORDER: AlertSeverity[] = ["critical", "warning", "info"];



export function groupBySeverity(rows: AlertItem[]): Record<AlertSeverity, AlertItem[]> {
  const groups: Record<AlertSeverity, AlertItem[]> = { critical: [], warning: [], info: [] };
  for (const row of rows) {
    if (row.severity in groups) {
      groups[row.severity].push(row);
    }
  }
  for (const sev of SEVERITY_ORDER) {
    groups[sev].sort((a, b) => b.opened_at.localeCompare(a.opened_at));
  }
  return groups;
}
