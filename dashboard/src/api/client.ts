





import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";

import type {
  AlertItem,
  CloneRepoArgs,
  CloneRepoOutcome,
  GithubConfigUpdate,
  GithubConfigView,
  GithubRepoHit,
  GithubTrackAllOutcome,
  ProjectDetail,
  ProjectListItem,
  PtySession,
  PtySpawnRequest,
  SetWebhooksBody,
  TrackAllOrgArgs,
  WebhooksView,
} from "./types";

const API_BASE = "/api/v1/dashboard";




const REFETCH_MS = 5_000;

export class ApiRequestError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
    this.name = "ApiRequestError";
  }
}

async function apiFetch<T>(path: string): Promise<T> {
  const resp = await fetch(`${API_BASE}${path}`, {
    headers: { Accept: "application/json" },
  });
  if (!resp.ok) {
    let message = `HTTP ${resp.status}`;
    try {
      const body = (await resp.json()) as { error?: string };
      if (body.error) message = body.error;
    } catch (error) {
      void error;
    }
    throw new ApiRequestError(resp.status, message);
  }
  return (await resp.json()) as T;
}

async function apiPost<T>(path: string, body?: unknown): Promise<T> {
  return apiWrite<T>("POST", path, body);
}

async function apiPut<T>(path: string, body?: unknown): Promise<T> {
  return apiWrite<T>("PUT", path, body);
}

async function apiWrite<T>(
  method: "POST" | "PUT",
  path: string,
  body?: unknown,
): Promise<T> {
  const resp = await fetch(`${API_BASE}${path}`, {
    method,
    headers: { Accept: "application/json", "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (!resp.ok) {
    let message = `HTTP ${resp.status}`;
    try {
      const parsed = (await resp.json()) as { error?: string };
      if (parsed.error) message = parsed.error;
    } catch (error) {
      void error;
    }
    throw new ApiRequestError(resp.status, message);
  }
  const text = await resp.text();
  return (text ? JSON.parse(text) : ({} as T)) as T;
}

export interface ActionResponse {
  stdout: string;
  stderr: string;
}




export function useProjects() {
  return useQuery<ProjectListItem[], ApiRequestError>({
    queryKey: ["dashboard", "projects"],
    queryFn: () => apiFetch<ProjectListItem[]>("/projects"),
    refetchInterval: REFETCH_MS,
    refetchIntervalInBackground: false,
  });
}




export function useProject(slug: string | null) {
  return useQuery<ProjectDetail, ApiRequestError>({
    queryKey: ["dashboard", "project", slug],
    queryFn: () => apiFetch<ProjectDetail>(`/projects/${slug}`),
    refetchInterval: REFETCH_MS,
    refetchIntervalInBackground: false,
    enabled: slug !== null,
  });
}




export function useAlerts() {
  return useQuery<AlertItem[], ApiRequestError>({
    queryKey: ["dashboard", "alerts"],
    queryFn: () => apiFetch<AlertItem[]>("/alerts"),
    refetchInterval: REFETCH_MS,
    refetchIntervalInBackground: false,
  });
}






function useProjectMutations(slug: string) {
  const client = useQueryClient();
  return (after: () => void = () => undefined) => {
    client.invalidateQueries({ queryKey: ["dashboard", "projects"] });
    client.invalidateQueries({ queryKey: ["dashboard", "project", slug] });
    client.invalidateQueries({ queryKey: ["dashboard", "alerts"] });
    after();
  };
}





export function useCloseIssue(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<ActionResponse, ApiRequestError, number>({
    mutationFn: (issueId: number) =>
      apiPost<ActionResponse>(`/w/${slug}/issues/${issueId}/close`),
    onSuccess: () => invalidate(),
  });
}


export function useReopenIssue(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<ActionResponse, ApiRequestError, number>({
    mutationFn: (issueId: number) =>
      apiPost<ActionResponse>(`/w/${slug}/issues/${issueId}/reopen`),
    onSuccess: () => invalidate(),
  });
}




export function useCommentIssue(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<
    ActionResponse,
    ApiRequestError,
    { issueId: number; content: string }
  >({
    mutationFn: ({ issueId, content }) =>
      apiPost<ActionResponse>(`/w/${slug}/issues/${issueId}/comment`, {
        content,
      }),
    onSuccess: () => invalidate(),
  });
}


export function useBlockIssue(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<
    ActionResponse,
    ApiRequestError,
    { issueId: number; blockerId: number }
  >({
    mutationFn: ({ issueId, blockerId }) =>
      apiPost<ActionResponse>(`/w/${slug}/issues/${issueId}/block`, {
        blocker_id: blockerId,
      }),
    onSuccess: () => invalidate(),
  });
}


export function useUnblockIssue(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<
    ActionResponse,
    ApiRequestError,
    { issueId: number; blockerId: number }
  >({
    mutationFn: ({ issueId, blockerId }) =>
      apiPost<ActionResponse>(`/w/${slug}/issues/${issueId}/unblock`, {
        blocker_id: blockerId,
      }),
    onSuccess: () => invalidate(),
  });
}


export function useRelateIssue(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<
    ActionResponse,
    ApiRequestError,
    { issueId: number; otherId: number }
  >({
    mutationFn: ({ issueId, otherId }) =>
      apiPost<ActionResponse>(`/w/${slug}/issues/${issueId}/relate`, {
        other_id: otherId,
      }),
    onSuccess: () => invalidate(),
  });
}


export function useLabelIssue(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<
    ActionResponse,
    ApiRequestError,
    { issueId: number; label: string }
  >({
    mutationFn: ({ issueId, label }) =>
      apiPost<ActionResponse>(`/w/${slug}/issues/${issueId}/label`, {
        label,
      }),
    onSuccess: () => invalidate(),
  });
}


export function useUnlabelIssue(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<
    ActionResponse,
    ApiRequestError,
    { issueId: number; label: string }
  >({
    mutationFn: ({ issueId, label }) =>
      apiPost<ActionResponse>(`/w/${slug}/issues/${issueId}/unlabel`, {
        label,
      }),
    onSuccess: () => invalidate(),
  });
}


export function useCreateMilestone(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<
    ActionResponse,
    ApiRequestError,
    { name: string; description?: string }
  >({
    mutationFn: ({ name, description }) =>
      apiPost<ActionResponse>(`/w/${slug}/milestones`, {
        name,
        description,
      }),
    onSuccess: () => invalidate(),
  });
}


export function useMilestoneAddIssue(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<
    ActionResponse,
    ApiRequestError,
    { milestoneId: number; issueId: number }
  >({
    mutationFn: ({ milestoneId, issueId }) =>
      apiPost<ActionResponse>(`/w/${slug}/milestones/${milestoneId}/add`, {
        issue_id: issueId,
      }),
    onSuccess: () => invalidate(),
  });
}


export function useMilestoneRemoveIssue(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<
    ActionResponse,
    ApiRequestError,
    { milestoneId: number; issueId: number }
  >({
    mutationFn: ({ milestoneId, issueId }) =>
      apiPost<ActionResponse>(`/w/${slug}/milestones/${milestoneId}/remove`, {
        issue_id: issueId,
      }),
    onSuccess: () => invalidate(),
  });
}


export function useCloseMilestone(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<ActionResponse, ApiRequestError, number>({
    mutationFn: (milestoneId: number) =>
      apiPost<ActionResponse>(`/w/${slug}/milestones/${milestoneId}/close`),
    onSuccess: () => invalidate(),
  });
}





export function useClaimLock(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<
    ActionResponse,
    ApiRequestError,
    { issueId: number; branch?: string }
  >({
    mutationFn: ({ issueId, branch }) =>
      apiPost<ActionResponse>(`/w/${slug}/locks/${issueId}/claim`, { branch }),
    onSuccess: () => invalidate(),
  });
}


export function useReleaseLock(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<ActionResponse, ApiRequestError, number>({
    mutationFn: (issueId: number) =>
      apiPost<ActionResponse>(`/w/${slug}/locks/${issueId}/release`),
    onSuccess: () => invalidate(),
  });
}



export function useStealLock(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<ActionResponse, ApiRequestError, number>({
    mutationFn: (issueId: number) =>
      apiPost<ActionResponse>(`/w/${slug}/locks/${issueId}/steal`),
    onSuccess: () => invalidate(),
  });
}



export function usePtySessions() {
  return useQuery<PtySession[], ApiRequestError>({
    queryKey: ["pty", "sessions"],
    queryFn: async () => {
      const resp = await fetch("/api/v1/pty/sessions", {
        headers: { Accept: "application/json" },
      });
      if (!resp.ok) {
        throw new ApiRequestError(resp.status, `HTTP ${resp.status}`);
      }
      return (await resp.json()) as PtySession[];
    },
    refetchInterval: REFETCH_MS,
  });
}



export function useSpawnPty() {
  const client = useQueryClient();
  return useMutation<PtySession, ApiRequestError, PtySpawnRequest>({
    mutationFn: async (req) => {
      const resp = await fetch("/api/v1/pty", {
        method: "POST",
        headers: {
          Accept: "application/json",
          "Content-Type": "application/json",
        },
        body: JSON.stringify(req),
      });
      if (!resp.ok) {
        let message = `HTTP ${resp.status}`;
        try {
          const body = (await resp.json()) as { error?: string };
          if (body.error) message = body.error;
        } catch (error) {
          void error;
        }
        throw new ApiRequestError(resp.status, message);
      }
      return (await resp.json()) as PtySession;
    },
    onSuccess: () =>
      client.invalidateQueries({ queryKey: ["pty", "sessions"] }),
  });
}



export function useGithubConfig() {
  return useQuery<GithubConfigView, ApiRequestError>({
    queryKey: ["dashboard", "github", "config"],
    queryFn: () => apiFetch<GithubConfigView>("/github/config"),
    refetchOnWindowFocus: false,
  });
}





export function useSetGithubConfig() {
  const client = useQueryClient();
  return useMutation<GithubConfigView, ApiRequestError, GithubConfigUpdate>({
    mutationFn: (body) => apiPost<GithubConfigView>("/github/config", body),
    onSuccess: (data) => {
      client.setQueryData(["dashboard", "github", "config"], data);
    },
  });
}



export function useOrgRepos(org: string | null, enabled: boolean) {
  return useQuery<GithubRepoHit[], ApiRequestError>({
    queryKey: ["dashboard", "github", "org-repos", org],
    queryFn: () =>
      apiFetch<GithubRepoHit[]>(
        `/github/orgs/${encodeURIComponent(org ?? "")}/repos`,
      ),
    enabled: enabled && !!org,
    refetchOnWindowFocus: false,
    staleTime: 60_000,
  });
}









export function useTrackAllOrg() {
  const client = useQueryClient();
  return useMutation<GithubTrackAllOutcome, ApiRequestError, TrackAllOrgArgs>({
    mutationFn: ({ org, cloneRoot, init, agentId }) =>






      apiPost<GithubTrackAllOutcome>(
        `/github/orgs/${encodeURIComponent(org)}/track-all`,
        {
          clone_root: cloneRoot || undefined,
          init: init || undefined,
          agent_id: agentId?.trim() || undefined,
        },
      ),
    onSuccess: () => {
      client.invalidateQueries({ queryKey: ["dashboard", "projects"] });
    },
  });
}






export function useSignBackfill(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<ActionResponse, ApiRequestError, void>({
    mutationFn: () =>
      apiPost<ActionResponse>(`/w/${slug}/integrity/sign-backfill`),
    onSuccess: () => invalidate(),
  });
}




export function useInitProject(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<ActionResponse, ApiRequestError, { agentId: string }>({
    mutationFn: ({ agentId }) =>
      apiPost<ActionResponse>(`/w/${slug}/init`, { agent_id: agentId }),
    onSuccess: () => invalidate(),
  });
}





export function useCloneRepo() {
  const client = useQueryClient();
  return useMutation<CloneRepoOutcome, ApiRequestError, CloneRepoArgs>({
    mutationFn: ({ url, slug, cloneRoot, init, agentId }) =>
      apiPost<CloneRepoOutcome>("/clone", {
        url,
        slug: slug?.trim() || undefined,
        clone_root: cloneRoot?.trim() || undefined,
        init: init || undefined,
        agent_id: agentId?.trim() || undefined,
      }),
    onSuccess: () => {
      client.invalidateQueries({ queryKey: ["dashboard", "projects"] });
    },
  });
}




export function useAgentRequest(slug: string) {
  const invalidate = useProjectMutations(slug);
  return useMutation<
    ActionResponse,
    ApiRequestError,
    {
      agentId: string;
      kind: "kill" | "pause" | "resume" | "reprioritise";
      subjectIssue?: number;
      reason?: string;
    }
  >({
    mutationFn: ({ agentId, kind, subjectIssue, reason }) =>
      apiPost<ActionResponse>(`/w/${slug}/agents/${agentId}/request`, {
        kind,
        subject_issue: subjectIssue,
        reason,
      }),
    onSuccess: () => invalidate(),
  });
}



export function useWebhooks() {
  return useQuery<WebhooksView, ApiRequestError>({
    queryKey: ["dashboard", "webhooks"],
    queryFn: () => apiFetch<WebhooksView>("/webhooks"),
    refetchOnWindowFocus: false,
  });
}




export function useSetWebhooks() {
  const client = useQueryClient();
  return useMutation<WebhooksView, ApiRequestError, SetWebhooksBody>({
    mutationFn: (body) => apiPut<WebhooksView>("/webhooks", body),
    onSuccess: (data) => {
      client.setQueryData(["dashboard", "webhooks"], data);
    },
  });
}
