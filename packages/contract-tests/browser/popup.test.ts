import type { Page } from "@playwright/test";
import { describe, expect, it, vi } from "vitest";
import { closePopup } from "./popup";

const closedMessage = "locator.click: Target page, context or browser has been closed";

function deferred() {
  let resolve = () => {};
  let reject = (_error: unknown) => {};
  const promise = new Promise<void>((done, fail) => {
    resolve = done;
    reject = fail;
  });
  return { promise, resolve, reject };
}

function fixture() {
  const clicked = deferred();
  const closed = deferred();
  const click = vi.fn(() => clicked.promise);
  const waitForEvent = vi.fn(() => closed.promise);
  const isVisible = vi.fn(async () => true);
  const popup = { waitForEvent, getByRole: () => ({ click }) } as unknown as Page;
  const opener = { getByRole: () => ({ isVisible }) } as unknown as Page;
  return { clicked, closed, click, waitForEvent, isVisible, popup, opener };
}

describe("closing a Connect popup", () => {
  it.each(["click", "close"] as const)("requires both signals when %s finishes first", async (first) => {
    const f = fixture();
    const done = vi.fn();
    const result = closePopup(f.popup, f.opener).then(done);
    expect(f.waitForEvent).toHaveBeenCalledWith("close");
    expect(f.click).toHaveBeenCalledWith({ noWaitAfter: true });
    (first === "click" ? f.clicked : f.closed).resolve();
    await Promise.resolve();
    expect(done).not.toHaveBeenCalled();
    expect(f.isVisible).not.toHaveBeenCalled();
    (first === "click" ? f.closed : f.clicked).resolve();
    await result;
    expect(f.isVisible).toHaveBeenCalledOnce();
    expect(done).toHaveBeenCalledOnce();
  });

  it.each(["click", "close"] as const)("accepts only the target-close race when %s arrives first", async (first) => {
    const f = fixture();
    const result = closePopup(f.popup, f.opener);
    const rejectClick = () => f.clicked.reject(new Error(`${closedMessage}\nCall log: private fixture data`));
    if (first === "click") rejectClick();
    else f.closed.resolve();
    await Promise.resolve();
    expect(f.isVisible).not.toHaveBeenCalled();
    if (first === "click") f.closed.resolve();
    else rejectClick();
    await result;
    expect(f.isVisible).toHaveBeenCalledOnce();
  });

  it.each([
    new Error("locator.click: Timeout 10000ms exceeded."),
    new Error("locator.click: unrelated error"),
    new Error(`${closedMessage}: another cause`),
    new Error(`unrelated error\n${closedMessage}`),
    { message: closedMessage },
  ])("preserves unrelated click failures even when the popup closed", async (error) => {
    const f = fixture();
    const result = closePopup(f.popup, f.opener);
    f.closed.resolve();
    f.clicked.reject(error);
    await expect(result).rejects.toBe(error);
    expect(f.isVisible).not.toHaveBeenCalled();
  });

  it("fails if the close event never arrives, even with the expected click error", async () => {
    const f = fixture();
    const error = new Error("Close event timed out.");
    const result = closePopup(f.popup, f.opener);
    f.clicked.reject(new Error(closedMessage));
    f.closed.reject(error);
    await expect(result).rejects.toBe(error);
  });

  it.each(["click", "close"] as const)("consumes a later rejection after %s fails first", async (first) => {
    const f = fixture();
    const error = new Error("First fixture failure.");
    const result = closePopup(f.popup, f.opener);
    (first === "click" ? f.clicked : f.closed).reject(error);
    await expect(result).rejects.toBe(error);
    (first === "click" ? f.closed : f.clicked).reject(new Error("Later fixture failure."));
    await new Promise((done) => setImmediate(done));
    expect(f.isVisible).not.toHaveBeenCalled();
  });

  it("refuses a missing opener control", async () => {
    const f = fixture();
    f.isVisible.mockResolvedValue(false);
    const result = closePopup(f.popup, f.opener);
    f.clicked.resolve();
    f.closed.resolve();
    await expect(result).rejects.toThrow("The Connect opener is no longer usable.");
  });

  it("preserves browser or context failures when checking the opener", async () => {
    const f = fixture();
    const error = new Error("Fixture context closed.");
    f.isVisible.mockRejectedValue(error);
    const result = closePopup(f.popup, f.opener);
    f.clicked.reject(new Error(closedMessage));
    f.closed.resolve();
    await expect(result).rejects.toBe(error);
  });
});
