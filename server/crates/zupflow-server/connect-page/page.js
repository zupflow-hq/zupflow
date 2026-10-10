// Zupflow's hosted Connect page. The server renders every state; this script
// only sends what the person does to the API and tells the embedding product
// when the session finished. It never builds markup from strings: everything
// it shows goes in as text.
(() => {
  const card = document.querySelector("[data-state]");
  if (!card) return;
  const state = card.getAttribute("data-state");

  // The link's reference is the page's own last path segment; it never
  // appears in the page itself.
  const reference = window.location.pathname.split("/").filter(Boolean).pop() || "";
  const api = `/v1/connect/sessions/${encodeURIComponent(reference)}`;

  /** Calls the hosted Connect API on this origin: `{ ok, status, data, error }`, or `null` when unreachable. */
  async function call(method, path, body) {
    const init = { method, headers: { Accept: "application/json" }, credentials: "omit", cache: "no-store" };
    if (body !== undefined) {
      init.headers["Content-Type"] = "application/json";
      init.body = JSON.stringify(body);
    }
    let response;
    try {
      response = await fetch(api + path, init);
    } catch {
      return null;
    }
    let envelope = null;
    try {
      envelope = await response.json();
    } catch {
      envelope = null;
    }
    return {
      ok: response.ok,
      status: response.status,
      data: envelope?.data ?? null,
      error: envelope?.error ?? null,
    };
  }

  const UNREACHABLE = "Zupflow couldn't be reached. Check your connection, then try again.";

  /** Shows the page again as the server sees the session now. */
  function reload() {
    window.location.replace(window.location.href);
  }

  function busy(button, on) {
    button.disabled = on;
    button.setAttribute("aria-busy", on ? "true" : "false");
  }

  function show(element, text) {
    if (!element) return;
    element.textContent = text;
    element.hidden = !text;
  }

  /** Whether the session is still waiting for the person; `null` when that can't be read. */
  async function stillPending() {
    const result = await call("GET", "");
    if (!result) return null;
    return result.ok && result.data?.status === "pending";
  }

  // --- Finished: tell the product, then go back -----------------------------

  function origins() {
    return (card.getAttribute("data-origins") || "").split(" ").filter(Boolean);
  }

  /** Posts to the window that opened or embeds this page, at the allowed origins only. */
  function notify() {
    const message = {
      type: "zupflow.connect.finished",
      session_id: card.getAttribute("data-session"),
      status: card.getAttribute("data-status"),
    };
    const targets = [];
    if (window.opener && window.opener !== window) targets.push(window.opener);
    if (window.parent && window.parent !== window) targets.push(window.parent);
    for (const target of targets) {
      for (const origin of origins()) {
        try {
          target.postMessage(message, origin);
        } catch {
          // Gone, or at another origin: the product still reads the session through the API.
        }
      }
    }
  }

  const framed = window.parent !== window;
  // Opened by another window: the product that opened it is still there, so
  // this window closes rather than taking the person back inside it.
  const popup = !framed && Boolean(window.opener) && window.opener !== window;
  if (state === "completed" || state === "failed" || state === "cancelled") {
    notify();
    const back = card.getAttribute("data-return");
    const note = card.querySelector(".outcome p");
    const close = card.querySelector("[data-action='close']");
    const closeNote = card.querySelector("[data-close-note]");

    /** Closes this window; if the browser won't, says so instead. */
    function closeWindow() {
      window.close();
      setTimeout(() => {
        if (window.closed) return;
        if (close) close.hidden = true;
        if (closeNote) closeNote.hidden = false;
      }, 400);
    }

    if (popup) {
      const backLink = card.querySelector("[data-action='back']");
      if (backLink) backLink.hidden = true;
      if (state === "completed") {
        show(note, "This window will close.");
        setTimeout(closeWindow, 1200);
      } else if (close) {
        close.hidden = false;
        close.addEventListener("click", closeWindow);
      }
    } else if (state === "completed" && back && framed) {
      // A frame can't take the person back; the product hears it finished.
      show(note, "You can continue in the app.");
    } else if (state === "completed" && back) {
      setTimeout(() => window.location.assign(back), 1200);
    }
    return;
  }

  if (state !== "pending") return;

  for (const element of card.querySelectorAll("[data-needs-js]")) element.disabled = false;
  const formError = card.querySelector("[data-form-error]");

  // --- Cancel ---------------------------------------------------------------

  const cancel = card.querySelector("[data-action='cancel']");
  cancel?.addEventListener("click", async () => {
    busy(cancel, true);
    const result = await call("POST", "/cancel");
    if (!result) {
      busy(cancel, false);
      show(formError, UNREACHABLE);
      return;
    }
    reload();
  });

  // --- A token app's credentials -------------------------------------------

  const form = card.querySelector("form[data-form='credentials']");
  if (form) {
    const submit = form.querySelector("button[type='submit']");
    const inputs = Array.from(form.querySelectorAll("input[data-key]"));
    const errorFor = (key) =>
      Array.from(form.querySelectorAll("[data-error-for]")).find((el) => el.getAttribute("data-error-for") === key);
    const hintFor = (key) =>
      Array.from(form.querySelectorAll("[data-hint-for]")).find((el) => el.getAttribute("data-hint-for") === key);

    function fieldErrors(fields) {
      for (const input of inputs) {
        const key = input.getAttribute("data-key");
        const message = fields && typeof fields[key] === "string" ? fields[key] : "";
        input.setAttribute("aria-invalid", message ? "true" : "false");
        show(errorFor(key), message);
        const hint = hintFor(key);
        if (hint) hint.hidden = Boolean(message);
      }
    }

    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      const values = {};
      for (const input of inputs) values[input.getAttribute("data-key")] = input.value;
      busy(submit, true);
      show(formError, "");
      const result = await call("POST", "/credentials", { values });
      if (result?.ok) {
        for (const input of inputs) input.value = "";
        reload();
        return;
      }
      if (!result) {
        busy(submit, false);
        show(formError, UNREACHABLE);
        return;
      }
      // A session that ended meanwhile shows why; a pending one keeps the form and its values.
      if (result.status === 404 || result.status === 409) {
        if ((await stillPending()) === false) {
          reload();
          return;
        }
      }
      busy(submit, false);
      const fields = result.error?.fields;
      fieldErrors(fields);
      const hasFields = fields && Object.keys(fields).some((key) => inputs.some((i) => i.dataset.key === key));
      show(formError, hasFields ? "" : result.error?.message || "Couldn't connect. Try again.");
    });
  }

  // --- An OAuth app's consent ----------------------------------------------

  const authorize = card.querySelector("[data-action='authorize']");
  if (authorize) {
    const label = authorize.querySelector("[data-label]");
    /** What the person can do about a refusal, as the server wrote it for this app. */
    const copyFor = (code) => {
      const copies = Array.from(card.querySelectorAll("[data-failure]"));
      const match = copies.find((el) => el.getAttribute("data-failure") === code);
      return (match ?? copies.find((el) => el.getAttribute("data-failure") === "default"))?.textContent ?? "";
    };

    authorize.addEventListener("click", async () => {
      busy(authorize, true);
      show(formError, "");
      const result = await call("POST", "/authorize");
      const url = result?.ok ? result.data?.authorization_url : null;
      if (typeof url === "string") {
        let target = null;
        try {
          target = new URL(url);
        } catch {
          target = null;
        }
        // The server only ever answers a provider's public HTTPS address.
        if (target && target.protocol === "https:") {
          window.location.assign(target.href);
          return;
        }
      }
      // A start the provider's side refused fails the session: the page shows why.
      const pending = await stillPending();
      if (pending === false) {
        reload();
        return;
      }
      busy(authorize, false);
      // Our side or the network failing can pass; anything else fails the same way again.
      const retry = !result || result.status >= 500 || pending === null;
      if (retry) {
        show(formError, result?.error?.message || UNREACHABLE);
        if (label) label.textContent = "Try again";
      } else {
        show(formError, copyFor(result.error?.code ?? ""));
        authorize.disabled = true;
      }
    });
  }
})();
