import { readonly, writable, type Readable } from "svelte/store";

const TOKEN_KEY = "rxh-token";

/** A non 2xx answer of the admin API, with the `error` text of its body. */
export class ApiError extends Error {
  status: number;

  constructor(status: number, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
  }
}

/** Reads a key of a storage of the browser: null where the browser blocks it. */
export function kept(storage: () => Storage, key: string): string | null {
  try {
    return storage().getItem(key);
  } catch {
    return null;
  }
}

/** Keeps `value` under a key, or drops the key for null; nothing where the browser blocks it. */
export function keep(storage: () => Storage, key: string, value: string | null): void {
  try {
    if (value === null) {
      storage().removeItem(key);
    } else {
      storage().setItem(key, value);
    }
  } catch {
    // Blocked storage: the page works, and forgets at the next load.
  }
}

const tokenWanted = writable(false);
let signedOut = false;
tokenWanted.subscribe((wanted) => (signedOut = wanted));

/** True after the API answered 401, until a token is given or somebody signs in. */
export const needsToken: Readable<boolean> = readonly(tokenWanted);

/**
 * True once `POST stop` was answered: the API is gone and the page stops
 * asking it anything.
 */
export const stopped = writable(false);

const tokensGiven = writable(0);

/** Keeps the token for this browser tab and sends it from now on. */
export function setToken(token: string): void {
  keep(() => sessionStorage, TOKEN_KEY, token);
  authorized();
}

/** Sends no token from now on: the cookie of a session says who asks. */
export function forgetToken(): void {
  keep(() => sessionStorage, TOKEN_KEY, null);
}

/**
 * Says the caller may be somebody else now (a token, a sign-in): the 401 is
 * forgotten and what `loadWithToken` watches is read again.
 */
export function authorized(): void {
  tokenWanted.set(false);
  tokensGiven.update((count) => count + 1);
}

/**
 * Calls `load` now, and again each time a token is given or somebody signs
 * in: what a tab reads once was refused before. Returns what stops it, for
 * `onMount`.
 */
export function loadWithToken(load: () => void): () => void {
  return tokensGiven.subscribe(() => load());
}

/**
 * Calls the admin API. `path` stays relative (`status`, not `/status`) so the
 * page works behind a path prefix.
 */
export async function api<T>(
  path: string,
  init: { method?: string; body?: unknown } = {},
): Promise<T> {
  // After a 401 only the way in is asked: the polls of the page wait for a sign-in.
  if (signedOut && !path.startsWith("auth/")) throw new ApiError(401, "Signed out.");
  const headers: Record<string, string> = {};
  const token = kept(() => sessionStorage, TOKEN_KEY);
  if (token) {
    headers.Authorization = `Bearer ${token}`;
  }
  let body: string | undefined;
  if (init.body !== undefined) {
    headers["Content-Type"] = "application/json";
    body = JSON.stringify(init.body);
  }
  const response = await fetch(path, { method: init.method ?? "GET", headers, body });
  if (!response.ok) {
    if (response.status === 401) {
      tokenWanted.set(true);
    }
    throw new ApiError(response.status, await errorText(response));
  }
  // The routes of the accounts answer 204: there is nothing to parse.
  if (response.status === 204) return undefined as T;
  return (await response.json()) as T;
}

async function errorText(response: Response): Promise<string> {
  try {
    const body: unknown = await response.json();
    if (body && typeof body === "object" && "error" in body && typeof body.error === "string") {
      return body.error;
    }
  } catch {
    // Not JSON: a proxy in front of the daemon answered.
  }
  return `${response.status} ${response.statusText}`.trim();
}
