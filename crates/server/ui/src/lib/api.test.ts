import { get } from "svelte/store";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { ApiError, api, authorized, needsToken, setToken } from "./api";

function answer(status: number, body: unknown) {
  const fetch = vi.fn(
    async (_url: string, _init: RequestInit) => new Response(JSON.stringify(body), { status }),
  );
  vi.stubGlobal("fetch", fetch);
  return fetch;
}

function headersOf(fetch: ReturnType<typeof answer>, call: number): Headers {
  return new Headers(fetch.mock.calls[call][1].headers);
}

beforeEach(() => sessionStorage.clear());
afterEach(() => vi.unstubAllGlobals());

test("sends_the_bearer_token", async () => {
  const fetch = answer(200, {});
  await api("status");
  expect(headersOf(fetch, 0).has("Authorization")).toBe(false);

  setToken("s3cret");
  await api("status");
  expect(headersOf(fetch, 1).get("Authorization")).toBe("Bearer s3cret");
});

test("relative_urls", async () => {
  const fetch = answer(200, { version: "1" });
  expect(await api<{ version: string }>("stats/history")).toEqual({ version: "1" });
  expect(fetch.mock.calls[0][0]).toBe("stats/history");

  await api("config/entities/upstream", { method: "POST", body: { policy: "random" } });
  const [url, init] = fetch.mock.calls[1];
  expect(url).toBe("config/entities/upstream");
  expect(init.method).toBe("POST");
  expect(init.body).toBe('{"policy":"random"}');
  expect(headersOf(fetch, 1).get("Content-Type")).toBe("application/json");
});

test("throws_the_error_text_of_the_body", async () => {
  answer(409, { error: "the file changed on disk" });
  const error = await api("config/files/0", { method: "PUT", body: "x" }).catch(
    (err: unknown) => err,
  );
  expect(error).toBeInstanceOf(ApiError);
  expect((error as ApiError).status).toBe(409);
  expect((error as ApiError).message).toBe("the file changed on disk");
  expect(get(needsToken)).toBe(false);
});

test("a_401_asks_for_the_token_and_rejects", async () => {
  setToken("stale");
  answer(401, { error: "admin API requires Authorization: Bearer <token>" });
  await expect(api("status")).rejects.toMatchObject({ status: 401 });
  expect(get(needsToken)).toBe(true);
  // The stored token stays until the user gives another one.
  expect(sessionStorage.getItem("rxh-token")).toBe("stale");

  setToken("fresh");
  expect(get(needsToken)).toBe(false);
});

test("after_a_401_only_the_way_in_is_asked", async () => {
  const fetch = answer(401, { error: "sign in first" });
  await expect(api("status")).rejects.toMatchObject({ status: 401 });
  await expect(api("stats/history")).rejects.toMatchObject({ status: 401 });
  expect(fetch).toHaveBeenCalledTimes(1);
  await api("auth/session").catch(() => undefined);
  expect(fetch).toHaveBeenCalledTimes(2);

  // Somebody signed in: the page asks again.
  authorized();
  await api("status").catch(() => undefined);
  expect(fetch).toHaveBeenCalledTimes(3);
  authorized();
});

test("an_answer_without_a_body_is_not_parsed", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => new Response(null, { status: 204 })));
  await expect(api("auth/logout", { method: "POST" })).resolves.toBeUndefined();
});
