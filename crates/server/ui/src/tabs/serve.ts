import { vi } from "vitest";
import rxhSchema from "../../../../../docs/rxh.schema.json";
import type { Session } from "../lib/session";

/** What `GET auth/session` answers: nobody, but for `changes`. */
export function who(changes: Partial<Session> = {}): Session {
  return {
    user: null,
    admin: false,
    roles: [],
    grants: [],
    creatable_groups: [],
    setup: false,
    setup_needs_code: false,
    password_login: true,
    oidc: false,
    oidc_issuer: null,
    // A user is one of the users file unless said otherwise, as most are.
    local: (changes.user ?? null) !== null,
    ...changes,
  };
}

/** Test helper: answers each `GET` with the body of its path, and every write with `write`. */
export function serve(answers: Record<string, unknown>, write: { status: number; body: unknown } = { status: 200, body: {} }) {
  const fetch = vi.fn(async (url: string, init: RequestInit) => {
    if (init.method !== "GET") {
      return new Response(write.status === 204 ? null : JSON.stringify(write.body), { status: write.status });
    }
    if (url === "schema") return Response.json(rxhSchema);
    return url in answers ? Response.json(answers[url]) : new Response("{}", { status: 404 });
  });
  vi.stubGlobal("fetch", fetch);
  return fetch;
}

/** The writes sent so far: method, path and JSON body. */
export function writes(fetch: ReturnType<typeof serve>) {
  return fetch.mock.calls
    .filter(([, init]) => init.method !== "GET")
    .map(([url, init]) => ({
      url,
      method: init.method,
      body: typeof init.body === "string" ? JSON.parse(init.body) : undefined,
    }));
}
