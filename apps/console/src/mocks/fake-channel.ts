/** What the mock's fake channel provider grants, shared by its handlers and the seed. */

/** The bot token scopes a generated app asks for, in Slack's terms: the server's `SLACK_SCOPES`. */
export const FAKE_SCOPES = [
  "app_mentions:read",
  "assistant:write",
  "channels:history",
  "channels:read",
  "chat:write",
  "chat:write.customize",
  "groups:history",
  "groups:read",
  "im:history",
  "im:read",
  "im:write",
  "mpim:history",
  "mpim:read",
  "reactions:write",
  "users:read",
  "users:read.email",
];
