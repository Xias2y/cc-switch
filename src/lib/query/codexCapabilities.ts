import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useTauriEvent } from "@/hooks/useTauriEvent";
import { proxyApi } from "@/lib/api/proxy";

const key = ["codexCapabilities"] as const;

export function useCodexCapabilities(enabled: boolean) {
  const client = useQueryClient();
  useTauriEvent("codex-capabilities-changed", () => {
    if (enabled) void client.invalidateQueries({ queryKey: key });
  });
  return useQuery({
    queryKey: key,
    queryFn: () => proxyApi.getCodexCapabilities(),
    enabled,
    staleTime: 15_000,
    // Local state only: this command never probes upstream providers.
    refetchInterval: enabled ? 30_000 : false,
    refetchIntervalInBackground: false,
  });
}
