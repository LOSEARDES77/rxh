import { fireEvent, render, screen, waitFor } from "@testing-library/svelte";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import rxhSchema from "../../../../docs/rxh.schema.json";
import App from "./App.svelte";
import { authorized, stopped } from "./lib/api";
import { session, type Session } from "./lib/session";
import { who } from "./tabs/serve";

beforeEach(() => {
  sessionStorage.clear();
  localStorage.clear();
  location.hash = "";
  session.set(undefined);
  authorized();
});
afterEach(() => vi.unstubAllGlobals());

const ANA = who({ user: "ana", admin: true, roles: ["admin"] });
const LUIS = who({ user: "luis", roles: ["web-editors"], creatable_groups: ["web"] });

/**
 * A daemon with accounts, as the page sees it: `caller` is who the cookie
 * says, and `account` who the password "correct horse" signs in.
 */
function daemon(caller: Session, account: Session = ANA) {
  const state = { caller };
  const fetch = vi.fn(async (url: string, init: RequestInit) => {
    const empty = new Response(null, { status: 204 });
    if (url === "auth/session") return Response.json(state.caller);
    if (url === "auth/login") {
      const { password } = JSON.parse(String(init.body));
      if (password !== "correct horse") return Response.json({ error: "wrong name or password" }, { status: 401 });
      state.caller = account;
      return empty;
    }
    if (url === "auth/logout") {
      state.caller = who();
      return empty;
    }
    if (url === "auth/setup") {
      state.caller = account;
      return empty;
    }
    if (!state.caller.admin && state.caller.user === null) {
      return Response.json({ error: "sign in first" }, { status: 401 });
    }
    if (url === "schema") return Response.json(rxhSchema);
    if (url === "certificates") return Response.json([]);
    if (url.startsWith("stats/history")) return Response.json({ interval_seconds: 5, buckets: [], samples: [] });
    if (url === "config/files") {
      return Response.json({ editable: true, files: [{ index: 0, path: "/etc/rxh/rxh.toml", revision: "r1", text: "" }], entities: [] });
    }
    return Response.json({ version: "1", uptime_seconds: 1, config_path: null, servers: [] });
  });
  vi.stubGlobal("fetch", fetch);
  return { fetch, state };
}

function type(label: string, text: string) {
  return fireEvent.input(screen.getByLabelText(label), { target: { value: text } });
}

function tabs(): (string | null)[] {
  return screen.getAllByRole("tab").map((tab) => tab.textContent);
}

function asked(fetch: ReturnType<typeof daemon>["fetch"], url: string): number {
  return fetch.mock.calls.filter(([asked]) => asked === url).length;
}

test("the_first_run_form_shows_when_setup_is_true", async () => {
  const { fetch } = daemon(who({ admin: true, setup: true }));
  const first = render(App);
  expect(await screen.findByRole("heading", { name: "Create the first user" })).toBeTruthy();
  // The API answers a local administrator without accounts: the form can wait.
  await fireEvent.click(screen.getByRole("button", { name: "Not now" }));
  expect(screen.queryByRole("heading", { name: "Create the first user" })).toBeNull();
  expect(tabs()).toHaveLength(5);
  first.unmount();

  // It stays away in this browser, and nothing was created.
  session.set(undefined);
  render(App);
  await screen.findByRole("tablist");
  expect(screen.queryByRole("heading", { name: "Create the first user" })).toBeNull();
  expect(fetch.mock.calls.some(([url]) => url === "auth/setup")).toBe(false);
});

test("the_first_run_form_cannot_be_skipped_by_who_needs_the_code", async () => {
  daemon(who({ setup: true, setup_needs_code: true }));
  render(App);
  expect(await screen.findByLabelText("Setup code")).toBeTruthy();
  expect(screen.queryByRole("button", { name: "Not now" })).toBeNull();
  expect(screen.queryByRole("tablist")).toBeNull();

  await type("Name", "ana");
  await type("Password", "correct horse");
  await type("Repeat the password", "correct horse");
  await type("Setup code", "k7m2xq9hpd");
  await fireEvent.click(screen.getByRole("button", { name: "Create user" }));
  expect(await screen.findByRole("tablist")).toBeTruthy();
  expect(screen.queryByLabelText("Setup code")).toBeNull();
});

test("not_now_does_not_hide_the_form_from_who_needs_the_code", async () => {
  localStorage.setItem("rxh-setup-skipped", "1");
  daemon(who({ setup: true, setup_needs_code: true }));
  render(App);
  expect(await screen.findByLabelText("Setup code")).toBeTruthy();
});

test("the_page_starts_where_storage_is_blocked", async () => {
  const blocked = new Proxy({}, { get: () => () => { throw new DOMException("blocked", "SecurityError"); } });
  vi.stubGlobal("localStorage", blocked);
  vi.stubGlobal("sessionStorage", blocked);
  daemon(who({ admin: true, setup: true }));
  render(App);
  await fireEvent.click(await screen.findByRole("button", { name: "Not now" }));
  expect(tabs()).toHaveLength(5);
});

test("no_form_and_no_user_menu_for_a_local_administrator", async () => {
  daemon(who({ admin: true }));
  render(App);
  await screen.findByRole("tablist");
  expect(tabs()).toHaveLength(5);
  expect(screen.queryByRole("button", { name: "Sign out" })).toBeNull();
  expect(screen.queryByRole("heading", { name: "Sign in" })).toBeNull();
});

test("a_401_shows_sign_in_and_keeps_the_open_form", async () => {
  location.hash = "hosts";
  const { fetch, state } = daemon(ANA);
  render(App);
  await fireEvent.click(await screen.findByRole("button", { name: "New host" }));
  await fireEvent.click(screen.getByRole("radio", { name: "TOML" }));
  await type("TOML of the table", 'listen = "127.0.0.1:8080"');

  // The session ended: the save is refused.
  state.caller = who();
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload" }));
  expect(await screen.findByRole("heading", { name: "Sign in" })).toBeTruthy();
  expect((screen.getByLabelText("TOML of the table") as HTMLTextAreaElement).value).toBe('listen = "127.0.0.1:8080"');
  // Signed out: no user menu, no refusal in the header, and the polls wait.
  expect(screen.queryByRole("button", { name: "Sign out" })).toBeNull();
  const before = fetch.mock.calls.length;
  document.dispatchEvent(new Event("visibilitychange"));
  await new Promise((done) => setTimeout(done, 20));
  expect(fetch.mock.calls.length).toBe(before);
  expect(document.querySelector('header [role="alert"]')).toBeNull();

  await type("Name", "ana");
  await type("Password", "correct horse");
  await fireEvent.click(screen.getByRole("button", { name: "Sign in" }));
  await waitFor(() => expect(screen.queryByRole("heading", { name: "Sign in" })).toBeNull());
  expect((screen.getByLabelText("TOML of the table") as HTMLTextAreaElement).value).toBe('listen = "127.0.0.1:8080"');
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload" }));
  await waitFor(() => expect(asked(fetch, "config/entities/server")).toBe(2));
});

test("another_user_signing_in_after_a_401_gets_nothing_of_the_one_before", async () => {
  location.hash = "hosts";
  const { fetch, state } = daemon(ANA, LUIS);
  render(App);
  await fireEvent.click(await screen.findByRole("button", { name: "New host" }));
  await fireEvent.click(screen.getByRole("radio", { name: "TOML" }));
  await type("TOML of the table", 'listen = "127.0.0.1:8080"');
  state.caller = who();
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload" }));
  await screen.findByRole("heading", { name: "Sign in" });
  const read = asked(fetch, "config/files");

  await type("Name", "luis");
  await type("Password", "correct horse");
  await fireEvent.click(screen.getByRole("button", { name: "Sign in" }));
  expect(await screen.findByText("luis")).toBeTruthy();
  // The tabs start again for luis: the form of ana is gone and the list is read anew.
  await waitFor(() => expect(screen.queryByLabelText("TOML of the table")).toBeNull());
  expect(tabs()).toEqual(["Overview", "Hosts"]);
  await waitFor(() => expect(asked(fetch, "config/files")).toBeGreaterThan(read));
  expect(await screen.findByRole("button", { name: "New host" })).toBeTruthy();
});

test("a_refused_sign_out_still_asks_who_the_page_is_for", async () => {
  const { fetch, state } = daemon(ANA);
  render(App);
  const out = await screen.findByRole("button", { name: "Sign out" });
  // The cookie expired: the daemon refuses the sign-out itself.
  state.caller = who();
  fetch.mockImplementationOnce(async () => Response.json({ error: "sign in first" }, { status: 401 }));
  await fireEvent.click(out);
  expect(await screen.findByRole("heading", { name: "Sign in" })).toBeTruthy();
  await waitFor(() => expect(screen.queryByRole("tablist")).toBeNull());
});

test("a_401_closes_the_password_form_for_good", async () => {
  const { state } = daemon(ANA);
  render(App);
  await fireEvent.click(await screen.findByRole("button", { name: "Change password" }));
  await type("Current password", "correct horse");
  state.caller = who();
  document.dispatchEvent(new Event("visibilitychange"));
  expect(await screen.findByRole("heading", { name: "Sign in" })).toBeTruthy();
  await type("Name", "ana");
  await type("Password", "correct horse");
  await fireEvent.click(screen.getByRole("button", { name: "Sign in" }));
  await screen.findByRole("button", { name: "Sign out" });
  // It does not come back empty: it is opened again by who wants it.
  expect(screen.queryByRole("heading", { name: "Change password" })).toBeNull();
  expect(screen.queryByLabelText("Current password")).toBeNull();
});

test("signing_in_reloads_the_data", async () => {
  location.hash = "hosts";
  const { fetch } = daemon(who());
  render(App);
  expect(await screen.findByRole("heading", { name: "Sign in" })).toBeTruthy();
  // Signed out, the page shows and asks nothing.
  expect(screen.queryByRole("tablist")).toBeNull();
  expect(asked(fetch, "config/files")).toBe(0);

  await type("Name", "ana");
  await type("Password", "wrong horse");
  await fireEvent.click(screen.getByRole("button", { name: "Sign in" }));
  expect((await screen.findByRole("alert")).textContent).toBe("wrong name or password");
  expect(screen.queryByRole("tablist")).toBeNull();

  await type("Password", "correct horse");
  await fireEvent.click(screen.getByRole("button", { name: "Sign in" }));
  expect(await screen.findByRole("button", { name: "New host" })).toBeTruthy();
  expect(screen.queryByRole("heading", { name: "Sign in" })).toBeNull();
  expect(screen.getByText("ana")).toBeTruthy();
  await waitFor(() => expect(asked(fetch, "status")).toBeGreaterThan(0));
  expect(asked(fetch, "config/files")).toBeGreaterThan(0);
});

test("a_user_without_admin_sees_two_tabs", async () => {
  location.hash = "settings";
  const { fetch } = daemon(LUIS);
  render(App);
  await screen.findByRole("tablist");
  expect(tabs()).toEqual(["Overview", "Hosts"]);
  // The address of a tab that is not shown opens the first one.
  expect(screen.getByRole("tab", { name: "Overview" }).getAttribute("aria-selected")).toBe("true");
  expect(screen.getByText("luis")).toBeTruthy();
  await waitFor(() => expect(asked(fetch, "stats/history")).toBe(1));
  // Nothing an administrator alone is answered.
  expect(asked(fetch, "certificates")).toBe(0);
});

test("sign_out_returns_to_the_sign_in_form", async () => {
  const { fetch } = daemon(ANA);
  render(App);
  await fireEvent.click(await screen.findByRole("button", { name: "Sign out" }));
  expect(await screen.findByRole("heading", { name: "Sign in" })).toBeTruthy();
  expect(asked(fetch, "auth/logout")).toBe(1);
  // What was on screen is the user's: it goes with the session.
  expect(screen.queryByRole("tablist")).toBeNull();
  expect(screen.queryByRole("button", { name: "Sign out" })).toBeNull();
});

test("change_password_opens_a_form_that_can_be_closed", async () => {
  daemon(ANA);
  render(App);
  await fireEvent.click(await screen.findByRole("button", { name: "Change password" }));
  expect(screen.getByLabelText("Current password")).toBeTruthy();
  await fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
  expect(screen.queryByLabelText("Current password")).toBeNull();
  expect(screen.getByRole("tablist")).toBeTruthy();
});

// jsdom has no modal dialogs: enough of them for the page.
HTMLDialogElement.prototype.showModal = function () {
  this.open = true;
};
HTMLDialogElement.prototype.close = function () {
  this.open = false;
  this.dispatchEvent(new Event("close"));
};

test("the_token_dialog_stays_open_until_a_token_is_given", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => Response.json({ error: "admin API requires a token" }, { status: 401 })),
  );
  const { container } = render(App);
  const dialog = container.ownerDocument.querySelector("dialog") as HTMLDialogElement;
  await waitFor(() => expect(dialog.open).toBe(true));

  // Escape fires `cancel`; closing anyway would leave the page without a way to give the token.
  const cancel = new Event("cancel", { cancelable: true });
  dialog.dispatchEvent(cancel);
  expect(cancel.defaultPrevented).toBe(true);
  dialog.close();
  await waitFor(() => expect(dialog.open).toBe(true));
});

test("once_stopped_the_page_says_so_and_stops_asking", async () => {
  vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
  // The overview tab reads the history too: an answer of another shape throws in it.
  const fetch = vi.fn(async (url: string) =>
    Response.json(
      url === "stats/history"
        ? { interval_seconds: 5, buckets: [], samples: [] }
        : url === "certificates"
          ? []
          : { version: "1", uptime_seconds: 1, config_path: null },
    ),
  );
  vi.stubGlobal("fetch", fetch);
  const { getByRole, queryByRole } = render(App);
  await waitFor(() => expect(fetch).toHaveBeenCalled());
  stopped.set(true);
  await waitFor(() => expect(getByRole("status").textContent).toContain("The daemon was stopped"));
  expect(queryByRole("tablist")).toBeNull();
  // The polls of the header and of the tabs ask nothing more.
  const before = fetch.mock.calls.length;
  vi.advanceTimersByTime(20_000);
  expect(fetch.mock.calls.length).toBe(before);
  stopped.set(false);
  vi.useRealTimers();
});
