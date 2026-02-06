import { useEffect, useRef } from "react";
import type { WorkspaceInfo } from "../../../types";

type WorkspaceRestoreOptions = {
  workspaces: WorkspaceInfo[];
  hasLoaded: boolean;
  connectWorkspace: (workspace: WorkspaceInfo) => Promise<void>;
  listThreadsForWorkspace: (
    workspace: WorkspaceInfo,
    options?: { preserveState?: boolean },
  ) => Promise<void>;
};

export function useWorkspaceRestore({
  workspaces,
  hasLoaded,
  connectWorkspace,
  listThreadsForWorkspace,
}: WorkspaceRestoreOptions) {
  const restoreSignatureByWorkspace = useRef<Record<string, string>>({});

  useEffect(() => {
    if (!hasLoaded) {
      return;
    }
    const signatures = restoreSignatureByWorkspace.current;
    const activeWorkspaceIds = new Set(workspaces.map((workspace) => workspace.id));
    Object.keys(signatures).forEach((workspaceId) => {
      if (!activeWorkspaceIds.has(workspaceId)) {
        delete signatures[workspaceId];
      }
    });

    workspaces.forEach((workspace) => {
      const nextSignature = [
        workspace.path,
        workspace.connected ? "1" : "0",
        workspace.settings.provider ?? "",
      ].join("|");
      if (signatures[workspace.id] === nextSignature) {
        return;
      }
      signatures[workspace.id] = nextSignature;
      void (async () => {
        try {
          if (!workspace.connected) {
            await connectWorkspace(workspace);
          }
          await listThreadsForWorkspace(workspace);
        } catch {
          // Silent: connection errors show in debug panel.
        }
      })();
    });
  }, [connectWorkspace, hasLoaded, listThreadsForWorkspace, workspaces]);
}
