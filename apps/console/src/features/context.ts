import { useSuspenseQuery } from "@tanstack/react-query";
import { notFound, useParams } from "@tanstack/react-router";
import type { Organization, Workspace, WorkspaceId } from "@zupflow/sdk";
import { useEffect } from "react";
import { viewerQuery, workspaceQuery } from "./queries";

/** The organization in the URL. Only valid under `/$org`. */
export function useOrg(): Organization {
  const { org: slug } = useParams({ from: "/_authed/$org" });
  const { data } = useSuspenseQuery(viewerQuery);
  const org = data.organizations.find((o) => o.slug === slug);
  if (!org) throw notFound();
  return org;
}

export function useViewerUser() {
  return useSuspenseQuery(viewerQuery).data.user;
}

/** The workspace in the URL. Only valid under `/$org/w/$ws`. */
export function useCurrentWorkspace(): Workspace {
  const { ws } = useParams({ from: "/_authed/$org/w/$ws" });
  return useSuspenseQuery(workspaceQuery(ws)).data;
}

/** The workspace in the URL, if any. Safe anywhere under `/$org`. */
export function useOptionalWorkspaceId(): WorkspaceId | undefined {
  return useParams({ strict: false }).ws;
}

export function canAdminister(org: Organization) {
  return org.role === "owner" || org.role === "admin";
}

/** Whether the member may change anything at all: every role but the read-only viewer. */
export function canChange(org: Organization) {
  return org.role !== "viewer";
}

export function useDocumentTitle(...parts: (string | undefined | null)[]) {
  const title = [...parts.filter(Boolean), "Zupflow Console"].join(" · ");
  useEffect(() => {
    document.title = title;
  }, [title]);
}
