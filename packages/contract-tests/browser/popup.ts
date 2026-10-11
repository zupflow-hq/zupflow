import type { Page } from "@playwright/test";

export async function closePopup(popup: Page, opener: Page) {
  const closed = popup.waitForEvent("close");
  const clicked = popup.getByRole("button", { name: "Close window", exact: true }).click({ noWaitAfter: true });
  await Promise.all([
    closed,
    clicked.catch((error: unknown) => {
      // Chromium can close the target before acknowledging the mouse release.
      // Playwright 1.64 gives that case this exact first line; never inspect or
      // print its call log. A timeout or any other click failure still fails.
      if (
        !(error instanceof Error) ||
        error.message.split("\n", 1)[0] !== "locator.click: Target page, context or browser has been closed"
      ) {
        throw error;
      }
    }),
  ]);
  // Closing the whole context or browser is not a successful popup close.
  // A live locator read also requires the embedding page to remain usable.
  if (!(await opener.getByRole("button", { name: "Open Connect", exact: true }).isVisible())) {
    throw new Error("The Connect opener is no longer usable.");
  }
}
