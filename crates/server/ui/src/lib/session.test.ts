import { get } from "svelte/store";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { serve, who, writes } from "../tabs/serve";
import { can, changePassword, refresh, session, setup, signIn, signOut, signedIn } from "./session";

beforeEach(() => {
  sessionStorage.clear();
  localStorage.clear();
  session.set(undefined);
});
afterEach(() => vi.unstubAllGlobals());

test("passwords_are_not_kept_in_stores_or_storage", async () => {
  sessionStorage.setItem("rxh-token", "stale");
  const fetch = serve({ "auth/session": who({ user: "ana", admin: true, roles: ["admin"] }) }, { status: 204, body: null });
  await signIn("ana", "correct horse");
  await changePassword("correct horse", "battery staple");
  await setup("ana", "first password", "k7m2xq9hpd");
  expect(writes(fetch)).toEqual([
    { url: "auth/login", method: "POST", body: { name: "ana", password: "correct horse" } },
    { url: "auth/password", method: "PUT", body: { current: "correct horse", new: "battery staple" } },
    { url: "auth/setup", method: "POST", body: { name: "ana", password: "first password", code: "k7m2xq9hpd" } },
  ]);
  expect(get(session)?.user).toBe("ana");
  const kept = JSON.stringify([get(session), { ...sessionStorage }, { ...localStorage }]);
  for (const secret of ["correct horse", "battery staple", "first password", "k7m2xq9hpd"]) {
    expect(kept).not.toContain(secret);
  }
  // A token would outrank the cookie of the session: it is dropped at sign-in.
  expect(sessionStorage.getItem("rxh-token")).toBeNull();
});

test("a_setup_without_a_code_sends_none", async () => {
  const fetch = serve({ "auth/session": who({ admin: true }) }, { status: 204, body: null });
  await setup("ana", "first password");
  expect(writes(fetch)[0].body).toEqual({ name: "ana", password: "first password" });
});

test("a_refused_sign_in_rejects_and_signs_nobody_in", async () => {
  serve({ "auth/session": who() }, { status: 429, body: { error: "too many attempts, wait a minute" } });
  await expect(signIn("ana", "wrong")).rejects.toMatchObject({ status: 429, message: "too many attempts, wait a minute" });
  expect(get(session)).toBeUndefined();
});

test("signing_out_reads_the_session_again", async () => {
  const answers = { "auth/session": who({ user: "ana", admin: true }) };
  const fetch = serve(answers, { status: 204, body: null });
  await refresh();
  expect(signedIn(get(session))).toBe(true);
  answers["auth/session"] = who();
  await signOut();
  expect(writes(fetch)).toEqual([{ url: "auth/logout", method: "POST", body: undefined }]);
  expect(signedIn(get(session))).toBe(false);
});

test("can_reads_the_permissions_the_daemon_gave", () => {
  expect(can({ permissions: ["view", "modify"] }, "modify")).toBe(true);
  expect(can({ permissions: ["view", "modify"] }, "delete")).toBe(false);
  expect(can({ permissions: [] }, "view")).toBe(false);
  // Pools and sections carry none: only an administrator gets them whole.
  expect(can({}, "delete")).toBe(true);
});
