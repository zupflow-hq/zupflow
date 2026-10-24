import type { ChannelAppManagerStatus, ChannelAppStatus, ChannelConnectionStatus } from "@zupflow/sdk";
import { StatusPill } from "@/components/app/status";
import { APP_STATUS, CONNECTION_STATUS, MANAGER_STATUS } from "./labels";

export function ManagerStatusPill({ status }: { status: ChannelAppManagerStatus }) {
  const [tone, label] = MANAGER_STATUS[status];
  return <StatusPill tone={tone}>{label}</StatusPill>;
}

export function ChannelAppStatusPill({ status }: { status: ChannelAppStatus }) {
  const [tone, label] = APP_STATUS[status];
  return <StatusPill tone={tone}>{label}</StatusPill>;
}

export function ConnectionStatusPill({ status }: { status: ChannelConnectionStatus }) {
  const [tone, label] = CONNECTION_STATUS[status];
  return <StatusPill tone={tone}>{label}</StatusPill>;
}
