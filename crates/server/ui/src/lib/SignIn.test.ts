import { fireEvent, render, screen } from "@testing-library/svelte";
import { get } from "svelte/store";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { serve, who, writes } from "../tabs/serve";
import { session } from "./session";
import SignIn from "./SignIn.svelte";

beforeEach(() => {
  sessionStorage.clear();
  session.set(undefined);
});
afterEach(() => vi.unstubAllGlobals());

function type(label: string, text: string) {
  return fireEvent.input(screen.getByLabelText(label), { target: { value: text } });
}

function value(label: string): string {
  return (screen.getByLabelText(label) as HTMLInputElement).value;
}

test("the_code_field_shows_only_when_needed", async () => {
  session.set(who({ admin: true, setup: true }));
  const first = render(SignIn, { form: "setup" });
  expect(screen.getByRole("heading", { name: "Create the first user" })).toBeTruthy();
  expect(screen.queryByLabelText("Setup code")).toBeNull();
  first.unmount();

  session.set(who({ setup: true, setup_needs_code: true }));
  const fetch = serve({ "auth/session": who({ user: "ana", admin: true }) }, { status: 204, body: null });
  render(SignIn, { form: "setup" });
  await type("Name", "ana");
  await type("Password", "first password");
  await type("Repeat the password", "first password");
  await type("Setup code", "k7m2xq9hpd");
  await fireEvent.click(screen.getByRole("button", { name: "Create user" }));
  await vi.waitFor(() => expect(get(session)?.user).toBe("ana"));
  expect(writes(fetch)).toEqual([
    { url: "auth/setup", method: "POST", body: { name: "ana", password: "first password", code: "k7m2xq9hpd" } },
  ]);
});

test("the_token_entry_is_still_reachable", async () => {
  session.set(who());
  const fetch = vi.fn(async (_url: string, init: RequestInit) => {
    const token = new Headers(init.headers).get("Authorization");
    return Response.json(who({ admin: token === "Bearer s3cret" }));
  });
  vi.stubGlobal("fetch", fetch);
  render(SignIn, { form: "sign-in" });
  expect(screen.getByLabelText("Password")).toBeTruthy();
  expect(screen.queryByLabelText("Token")).toBeNull();
  await fireEvent.click(screen.getByRole("button", { name: "Use the admin token" }));
  expect(screen.queryByLabelText("Password")).toBeNull();

  await type("Token", "wrong");
  await fireEvent.click(screen.getByRole("button", { name: "Use token" }));
  expect((await screen.findByRole("alert")).textContent).toContain("did not accept this token");

  await type("Token", "s3cret");
  await fireEvent.click(screen.getByRole("button", { name: "Use token" }));
  await vi.waitFor(() => expect(get(session)?.admin).toBe(true));
  expect(sessionStorage.getItem("rxh-token")).toBe("s3cret");

  // Back to the password, where the daemon takes one.
  await fireEvent.click(screen.getByRole("button", { name: "Sign in with a password" }));
  expect(screen.getByLabelText("Password")).toBeTruthy();
});

test("over_plain_http_without_password_sign_in_the_page_points_to_https", async () => {
  // jsdom serves the page over http:, as a plain HTTP admin listener would.
  session.set(who({ password_login: false, tls: true, oidc: true, oidc_issuer: "id.example.com" }));
  render(SignIn, { form: "sign-in" });
  expect(screen.getByText("Open this page over https to sign in.")).toBeTruthy();
  const https = location.href.replace(/^http:/, "https:");
  const link = screen.getByRole("link", { name: https });
  expect(link.getAttribute("href")).toBe(https);
  expect(screen.queryByLabelText("Password")).toBeNull();
  // The provider takes no sign-in where no session can be.
  expect(screen.queryByRole("link", { name: "Sign in with id.example.com" })).toBeNull();

  await fireEvent.click(screen.getByRole("button", { name: "Use the admin token" }));
  expect(screen.getByLabelText("Token")).toBeTruthy();
  expect(screen.queryByText("Open this page over https to sign in.")).toBeNull();
  // No password form is behind it: the way back is to the sentence.
  expect(screen.queryByRole("button", { name: "Sign in with a password" })).toBeNull();
  await fireEvent.click(screen.getByRole("button", { name: "Back" }));
  expect(screen.getByText("Open this page over https to sign in.")).toBeTruthy();
});

test("without_tls_on_the_listener_only_the_token_is_asked", () => {
  // Plain HTTP with no HTTPS to go to: a link there would lead nowhere.
  session.set(who({ password_login: false, tls: false }));
  render(SignIn, { form: "sign-in" });
  expect(screen.getByLabelText("Token")).toBeTruthy();
  expect(screen.queryByText("Open this page over https to sign in.")).toBeNull();
  expect(screen.queryByRole("button", { name: "Sign in with a password" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Back" })).toBeNull();
});

test("the_provider_button_shows_on_a_401_with_a_stale_session", () => {
  // The cookie ended, but the page has not read the session again: it still names the user.
  session.set(who({ user: "ana", oidc: true, oidc_issuer: "id.example.com" }));
  render(SignIn, { form: "sign-in" });
  expect(screen.getByRole("link", { name: "Sign in with id.example.com" })).toBeTruthy();
});

test("the_provider_button_shows_when_oidc_is_on", () => {
  session.set(who({ oidc: true, oidc_issuer: "id.example.com" }));
  const on = render(SignIn, { form: "sign-in" });
  expect(screen.getByRole("link", { name: "Sign in with id.example.com" })).toBeTruthy();
  // The password form stays under it.
  expect(screen.getByLabelText("Password")).toBeTruthy();
  on.unmount();

  session.set(who());
  render(SignIn, { form: "sign-in" });
  expect(screen.queryByRole("link", { name: /Sign in with/ })).toBeNull();
});

test("it_links_to_the_login_route", () => {
  session.set(who({ oidc: true, oidc_issuer: "id.example.com" }));
  render(SignIn, { form: "sign-in" });
  const link = screen.getByRole("link", { name: "Sign in with id.example.com" });
  expect(link.getAttribute("href")).toBe("auth/oidc/login");
  expect(link.className).toContain("primary");
});

test("the_provider_is_offered_while_no_user_exists", () => {
  // An administrator of the provider needs no local user to sign in.
  session.set(who({ setup: true, setup_needs_code: true, oidc: true, oidc_issuer: "id.example.com" }));
  render(SignIn, { form: "setup" });
  expect(screen.getByRole("link", { name: "Sign in with id.example.com" })).toBeTruthy();
  expect(screen.getByLabelText("Setup code")).toBeTruthy();
});

test("no_password_is_offered_while_no_user_exists", () => {
  session.set(who({ setup: true }));
  render(SignIn, { form: "sign-in" });
  expect(screen.getByLabelText("Token")).toBeTruthy();
  expect(screen.queryByRole("button", { name: "Sign in with a password" })).toBeNull();
});

test("the_password_form_names_the_user_for_password_managers", () => {
  session.set(who({ user: "ana", admin: true }));
  const { container } = render(SignIn, { form: "password" });
  const name = container.querySelector('input[autocomplete="username"]') as HTMLInputElement;
  expect(name.value).toBe("ana");
  expect(name.className).toContain("visually-hidden");
  expect(name.hasAttribute("style")).toBe(false);
});

test("a_refused_sign_in_shows_the_message_of_the_daemon", async () => {
  session.set(who());
  serve({ "auth/session": who() }, { status: 429, body: { error: "too many attempts, wait a minute" } });
  render(SignIn, { form: "sign-in" });
  await type("Name", "ana");
  await type("Password", "wrong password");
  await fireEvent.click(screen.getByRole("button", { name: "Sign in" }));
  expect((await screen.findByRole("alert")).textContent).toBe("too many attempts, wait a minute");
  expect(value("Name")).toBe("ana");
  expect(value("Password")).toBe("");
});

test("a_new_password_is_sent_only_when_typed_twice_the_same", async () => {
  session.set(who({ user: "ana", admin: true }));
  const fetch = serve({}, { status: 204, body: null });
  const ondone = vi.fn();
  const oncancel = vi.fn();
  render(SignIn, { form: "password", ondone, oncancel });
  await type("Current password", "correct horse");
  await type("New password", "battery staple");
  await type("Repeat the password", "battery stapel");
  await fireEvent.click(screen.getByRole("button", { name: "Save password" }));
  expect((await screen.findByRole("alert")).textContent).toContain("differ");
  expect(writes(fetch)).toEqual([]);

  await type("Repeat the password", "battery staple");
  await fireEvent.click(screen.getByRole("button", { name: "Save password" }));
  await vi.waitFor(() => expect(ondone).toHaveBeenCalled());
  expect(writes(fetch)).toEqual([
    { url: "auth/password", method: "PUT", body: { current: "correct horse", new: "battery staple" } },
  ]);
  await fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
  expect(oncancel).toHaveBeenCalled();
});
