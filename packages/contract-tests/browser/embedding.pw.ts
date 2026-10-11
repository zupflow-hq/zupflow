import { embedder, finishedMessage, messageCount, prepare } from "./embedder";
import { createInput, expect, must, test } from "./fixtures";
import { closePopup } from "./popup";
import { completed, consent, headers, token } from "./ui";

test("popup-success", async ({ stack, flow }) => {
  const host = await embedder();
  try {
    const { organization, workspaces, apps } = stack.fixture;
    const workspace = workspaces.qaPlayground.id;
    await stack.owner.connectSettings.update(organization, "sandbox", {
      return_urls: [],
      allowed_origins: [host.origin],
    });
    const session = await stack.session(organization, workspace, createInput(apps.echoServer));
    await prepare(flow.page, host.origin, session.url, stack.baseURL);
    const popupEvent = flow.page.waitForEvent("popup");
    await flow.page.getByRole("button", { name: "Open Connect", exact: true }).click();
    const popup = await popupEvent;
    const closed = popup.waitForEvent("close");
    await token(popup);
    await finishedMessage(flow.page, session.id, "completed");
    await closed;
    await completed(stack, organization, workspace, session.id);
    expect(new URL(flow.page.url()).origin === host.origin).toBe(true);
  } finally {
    await host.close();
  }
});

test("popup-cancel", async ({ stack, flow }) => {
  const allowed = await embedder();
  const unlisted = await embedder();
  try {
    const { organization, workspaces, apps } = stack.fixture;
    const workspace = workspaces.qaPlayground.id;
    await stack.owner.connectSettings.update(organization, "sandbox", {
      return_urls: [],
      allowed_origins: [allowed.origin],
    });
    // The allowed opener gets the terminal hint and an explicit close action.
    const session = await stack.session(organization, workspace, createInput(apps.echoServer));
    await prepare(flow.page, allowed.origin, session.url, stack.baseURL);
    const popupEvent = flow.page.waitForEvent("popup");
    await flow.page.getByRole("button", { name: "Open Connect", exact: true }).click();
    const popup = await popupEvent;
    await popup.getByRole("button", { name: "Cancel", exact: true }).click();
    await finishedMessage(flow.page, session.id, "cancelled");
    await test.step("popup-cancel-close", () => closePopup(popup, flow.page));
    // An unlisted origin can open a bearer link, but cannot hear its outcome.
    const other = await stack.session(organization, workspace, createInput(apps.echoServer));
    await prepare(flow.page, unlisted.origin, other.url, stack.baseURL);
    const otherEvent = flow.page.waitForEvent("popup");
    await flow.page.getByRole("button", { name: "Open Connect", exact: true }).click();
    const otherPopup = await otherEvent;
    await otherPopup.getByRole("button", { name: "Cancel", exact: true }).click();
    await expect(otherPopup.getByRole("button", { name: "Close window", exact: true })).toBeVisible();
    await test.step("popup-unlisted-close", () => closePopup(otherPopup, flow.page));
    expect(await messageCount(flow.page)).toBe(0);
  } finally {
    await unlisted.close();
    await allowed.close();
  }
});

test.describe("OAuth popup", () => {
  test.use({ mode: "oauth" });
  test("popup-denied", async ({ stack, flow }) => {
    const host = await embedder();
    try {
      const { organization, workspaces } = stack.fixture;
      const workspace = workspaces.qaPlayground.id;
      await stack.owner.connectSettings.update(organization, "sandbox", {
        return_urls: [],
        allowed_origins: [host.origin],
      });
      const session = await stack.session(organization, workspace, createInput(must(stack.taskboard, "Taskboard").app));
      await prepare(flow.page, host.origin, session.url, stack.baseURL);
      const popupEvent = flow.page.waitForEvent("popup");
      await flow.page.getByRole("button", { name: "Open Connect", exact: true }).click();
      const popup = await popupEvent;
      await consent(popup);
      await popup.getByRole("button", { name: "Deny", exact: true }).click();
      await finishedMessage(flow.page, session.id, "failed");
      await expect(popup.getByRole("heading", { name: "Access wasn't granted", exact: true })).toBeVisible();
      await test.step("popup-denied-close", () => closePopup(popup, flow.page));
      await test.step("popup-denied-outcome", async () => {
        expect((await stack.owner.connectSessions.get(workspace, session.id)).failure_code).toBe("consent_denied");
      });
    } finally {
      await host.close();
    }
  });
});

test("frame-allowlist", async ({ stack, flow }) => {
  const allowed = await embedder();
  const denied = await embedder();
  try {
    const { organization, workspaces, apps } = stack.fixture;
    const workspace = workspaces.qaPlayground.id;
    const returnURL = `${allowed.origin}/return`;
    await stack.owner.connectSettings.update(organization, "sandbox", {
      return_urls: [returnURL],
      allowed_origins: [allowed.origin],
    });
    const session = await stack.session(organization, workspace, {
      ...createInput(apps.echoServer),
      return_url: returnURL,
    });
    await prepare(flow.page, allowed.origin, session.url, stack.baseURL);
    const pageResponse = flow.page.waitForResponse((response) =>
      new URL(response.url()).pathname.startsWith("/connect/"),
    );
    await flow.page.getByRole("button", { name: "Embed Connect", exact: true }).click();
    await headers(await pageResponse, true);
    const frame = flow.page.frameLocator("iframe");
    await frame.getByLabel("Token", { exact: true }).fill("framed-browser-fixture-token");
    await frame.getByRole("button", { name: "Connect", exact: true }).click();
    await expect(frame.getByRole("heading", { name: "Echo test server connected", exact: true })).toBeVisible();
    await expect(frame.getByText("You can continue in the app.", { exact: true })).toBeVisible();
    await finishedMessage(flow.page, session.id, "completed");
    await completed(stack, organization, workspace, session.id);
    expect(new URL(flow.page.url()).pathname).toBe("/");
    const neighbor = await stack.session(organization, workspace, createInput(apps.echoServer));
    await prepare(flow.page, denied.origin, neighbor.url, stack.baseURL);
    let browserRefusedFrame = false;
    flow.page.on("console", (message) => {
      // Keep only a boolean. Chromium's diagnostic itself contains the link.
      if (message.type() === "error" && message.text().includes("frame-ancestors")) browserRefusedFrame = true;
    });
    flow.expectFrameRefusal();
    const refused = flow.page.waitForResponse((response) => new URL(response.url()).pathname.startsWith("/connect/"));
    await flow.page.getByRole("button", { name: "Embed Connect", exact: true }).click();
    expect((await refused).headers()["content-security-policy"]?.includes(`frame-ancestors ${allowed.origin}`)).toBe(
      true,
    );
    await expect.poll(() => browserRefusedFrame).toBe(true);
    await expect(
      flow.page.frameLocator("iframe").getByRole("heading", { name: "Connect Echo test server" }),
    ).toHaveCount(0);
    expect(await messageCount(flow.page)).toBe(0);
    expect((await stack.owner.connectSessions.get(workspace, neighbor.id)).status).toBe("pending");
  } finally {
    await denied.close();
    await allowed.close();
  }
});

test("return-allowlist", async ({ stack, flow }) => {
  const host = await embedder();
  try {
    const { organization, workspaces, apps } = stack.fixture;
    const workspace = workspaces.qaPlayground.id;
    const returnURL = `${host.origin}/return?keep=yes&zupflow_session=old&zupflow_session=older`;
    await stack.owner.connectSettings.update(organization, "sandbox", {
      return_urls: [returnURL],
      allowed_origins: [],
    });
    const session = await stack.session(organization, workspace, {
      ...createInput(apps.echoServer),
      return_url: returnURL,
    });
    await flow.page.goto(session.url);
    await token(flow.page);
    await expect(flow.page.getByRole("heading", { name: "Back in the app", exact: true })).toBeVisible();
    const returned = new URL(flow.page.url());
    expect(returned.origin === host.origin && returned.pathname === "/return").toBe(true);
    expect(returned.searchParams.getAll("zupflow_session")).toEqual([session.id]);
    expect(returned.searchParams.get("keep")).toBe("yes");
    await completed(stack, organization, workspace, session.id);
    const removed = await stack.session(organization, workspace, {
      ...createInput(apps.echoServer),
      return_url: returnURL,
    });
    await flow.page.goto(removed.url);
    await stack.owner.connectSettings.update(organization, "sandbox", { return_urls: [], allowed_origins: [] });
    await token(flow.page);
    await expect(flow.page.locator("[data-state='completed']")).toBeVisible();
    expect(await flow.page.locator("[data-state]").getAttribute("data-return")).toBeNull();
    await expect(flow.page.locator("[data-action='back']")).toHaveCount(0);
    await completed(stack, organization, workspace, removed.id);
    expect(new URL(flow.page.url()).origin === stack.baseURL).toBe(true);
  } finally {
    await host.close();
  }
});
