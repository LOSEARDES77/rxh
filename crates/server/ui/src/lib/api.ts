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

const tokenWanted = writable(false);

/** True after the API answered 401, until a token is given. */
export const needsToken: Readable<boolean> = readonly(tokenWanted);

/**
 * True once `POST stop` was answered: the API is gone and the page stops
 * asking it anything.
 */
export const stopped = writable(false);

const tokensGiven = writable(0);

/** Keeps the token for this browser tab and sends it from now on. */
export function setToken(token: string): void {
  sessionStorage.setItem(TOKEN_KEY, token);
  tokenWanted.set(false);
  tokensGiven.update((count) => count + 1);
}

/**
 * Calls `load` now, and again each time a token is given: what a tab reads
 * once was refused without it. Returns what stops it, for `onMount`.
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
  const headers: Record<string, string> = {};
  const token = sessionStorage.getItem(TOKEN_KEY);
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
