import { get, writable } from "svelte/store";
import { api, authorized, forgetToken, needsToken } from "./api";

export type Permission = "view" | "modify" | "create" | "delete";

/** What a role allows on the hosts of a group, or on one host. */
export type Grant = { group?: string; host?: string; allow: Permission[] };

/** The answer of `GET auth/session`: who the daemon takes this page for. */
export type Session = {
  /** Null for who is not signed in, the token and a local caller included. */
  user: string | null;
  admin: boolean;
  roles: string[];
  /** Empty for an administrator, who may do everything. */
  grants: Grant[];
  creatable_groups: string[];
  /** No user exists yet. */
  setup: boolean;
  /** The first user needs the setup code from this caller, which only HTTPS takes. */
  setup_needs_code: boolean;
  /** Whether `POST auth/login` works on this connection. */
  password_login: boolean;
  /** Whether `auth/oidc/login` signs in through a provider. */
  oidc: boolean;
  /** The host of the provider, to name it. */
  oidc_issuer: string | null;
  /** A user of the users file, who has a password here; false for one of the provider. */
  local: boolean;
  /** Whether the admin listener has `[admin] tls`, whatever this page was loaded over. */
  tls: boolean;
};

/** Undefined until the daemon answered. Passwords never get here. */
export const session = writable<Session | undefined>();

/** True for a caller the API answers: an administrator or a user. */
export function signedIn(who: Session | undefined): boolean {
  return who !== undefined && (who.admin || who.user !== null);
}

export async function refresh(): Promise<void> {
  const who = await api<Session>("auth/session");
  session.set(who);
  // A local administrator whose cookie expired is somebody all the same: the 401 is over.
  if (signedIn(who) && get(needsToken)) authorized();
}

export async function signIn(name: string, password: string): Promise<void> {
  await api("auth/login", { method: "POST", body: { name, password } });
  // A token outranks the cookie: with one kept, the page would not be the user.
  forgetToken();
  await refresh();
  authorized();
}

export async function signOut(): Promise<void> {
  try {
    await api("auth/logout", { method: "POST" });
  } finally {
    // Refused or not, who the page is for is asked again.
    await refresh();
  }
}

/** Creates the first user, and signs it in where the daemon gives a session. */
export async function setup(name: string, password: string, code?: string): Promise<void> {
  await api("auth/setup", { method: "POST", body: { name, password, ...(code ? { code } : {}) } });
  await refresh();
  authorized();
}

export async function changePassword(current: string, next: string): Promise<void> {
  await api("auth/password", { method: "PUT", body: { current, new: next } });
}

/**
 * Whether the daemon lets the caller do `permission` with `entity`. Only
 * servers carry permissions: pools and sections come whole to
 * administrators alone, and the daemon has the last word anyway.
 */
export function can(entity: { permissions?: Permission[] }, permission: Permission): boolean {
  return entity.permissions?.includes(permission) ?? true;
}
