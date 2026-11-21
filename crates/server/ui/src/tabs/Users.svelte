<script lang="ts">
  import { onMount } from "svelte";
  import { ApiError, api, loadWithToken } from "../lib/api";
  import type { Files } from "../lib/schema";
  import { refresh as readSession, session, type Grant, type Permission } from "../lib/session";

  type User = { name: string; roles: string[] };
  // `GET roles` lists the built-in `admin` first, without grants.
  type Role = { name: string; builtin?: boolean; grant?: Grant[] };
  type Row = { kind: "group" | "host"; target: string; allow: Permission[] };

  const PERMISSIONS: Permission[] = ["view", "modify", "create", "delete"];
  const LABELS = { view: "View", modify: "Modify", create: "Create", delete: "Delete" };

  let users: User[] = $state.raw([]);
  let roles: Role[] = $state.raw([]);
  let files: Files | undefined = $state.raw();
  // Of the last read; a later read that works clears it.
  let failure = $state("");
  let notice = $state("");
  // The refusals of the daemon, by what they are about: they stay until it is sent again.
  let refusals: Record<string, string> = $state({});
  // The roles ticked in a row and not saved yet, by user.
  let drafts: Record<string, string[]> = $state({});
  let adding: { name: string; password: string; repeat: string; roles: string[] } | undefined = $state();
  let passwordFor: { name: string; password: string; repeat: string } | undefined = $state();
  let editing: { name: string; created: boolean; rows: Row[] } | undefined = $state();

  const me = $derived($session?.user ?? null);
  // What a grant row offers; a role may name a group or a host that does not exist yet.
  const servers = $derived((files?.entities ?? []).filter((entity) => entity.kind === "server"));
  const groups = $derived(
    [...new Set(servers.flatMap((entity) => [entity.value.groups ?? []].flat().map(String)))].sort(),
  );
  const hosts = $derived(
    [
      ...new Set(
        servers
          .map((entity) => entity.value.name)
          .filter((name) => typeof name === "string" && !name.includes("{")),
      ),
    ].sort() as string[],
  );

  function message(err: unknown): string {
    return err instanceof Error ? err.message : String(err);
  }

  async function load() {
    try {
      [users, roles, files] = await Promise.all([
        api<User[]>("users"),
        api<Role[]>("roles"),
        // Only suggestions: the accounts are managed without them.
        api<Files>("config/files").catch(() => undefined),
      ]);
      failure = "";
    } catch (err) {
      // Refused because signed out: the sign-in form says so.
      failure = err instanceof ApiError && err.status === 401 ? "" : message(err);
    }
  }

  /** Sends a change; a refusal is kept under `about` and what was typed stays. */
  async function send(about: string, work: () => Promise<unknown>, done: string): Promise<boolean> {
    delete refusals[about];
    notice = "";
    try {
      await work();
      notice = done;
    } catch (err) {
      refusals[about] = message(err);
      return false;
    }
    await load();
    return true;
  }

  function path(kind: "users" | "roles", name: string): string {
    return `${kind}/${encodeURIComponent(name)}`;
  }

  function summary(grant: Grant): string {
    return grant.host !== undefined
      ? `host ${grant.host}: ${grant.allow.join(", ")}`
      : `group ${grant.group}: ${grant.allow.join(", ")}`;
  }

  function rolesOf(user: User): string[] {
    return drafts[user.name] ?? user.roles;
  }

  function toggle(list: string[], role: string, on: boolean): string[] {
    return on ? [...list, role] : list.filter((other) => other !== role);
  }

  async function saveRoles(user: User) {
    const next = rolesOf(user);
    const own = user.name === me;
    if (
      own &&
      user.roles.includes("admin") &&
      !next.includes("admin") &&
      !confirm(
        "Take the admin role from yourself? You lose this tab and every right of an administrator at once.",
      )
    ) {
      return;
    }
    const saved = await send(
      `user:${user.name}`,
      () => api(path("users", user.name), { method: "PUT", body: { roles: next } }),
      `Roles of ${user.name} saved.`,
    );
    if (!saved) return;
    delete drafts[user.name];
    // Who the page is for changed: the tabs follow.
    if (own) await readSession().catch(() => undefined);
  }

  async function removeUser(user: User) {
    const own = user.name === me;
    const question = own
      ? `Remove your own user ${user.name}? You are signed out and lose this tab at once.`
      : `Remove user ${user.name}? Its sessions end.`;
    if (!confirm(question)) return;
    const removed = await send(
      `user:${user.name}`,
      () => api(path("users", user.name), { method: "DELETE" }),
      `User ${user.name} removed.`,
    );
    if (removed && own) await readSession().catch(() => undefined);
  }

  async function addUser(event: SubmitEvent) {
    event.preventDefault();
    if (!adding) return;
    const { name, password, repeat, roles: chosen } = adding;
    if (password !== repeat) {
      refusals["new-user"] = "The two passwords differ.";
      return;
    }
    const body = { name, password, roles: chosen };
    if (await send("new-user", () => api("users", { method: "POST", body }), `User ${name} added.`)) {
      adding = undefined;
    }
  }

  async function setPassword(event: SubmitEvent) {
    event.preventDefault();
    if (!passwordFor) return;
    const { name, password, repeat } = passwordFor;
    if (password !== repeat) {
      refusals.password = "The two passwords differ.";
      return;
    }
    const body = { password };
    if (await send("password", () => api(path("users", name), { method: "PUT", body }), `Password of ${name} set.`)) {
      passwordFor = undefined;
    }
  }

  function edit(role?: Role) {
    delete refusals.role;
    notice = "";
    const rows: Row[] = (role?.grant ?? []).map((grant) =>
      grant.host !== undefined
        ? { kind: "host", target: grant.host, allow: [...grant.allow] }
        : { kind: "group", target: grant.group ?? "", allow: [...grant.allow] },
    );
    editing = { name: role?.name ?? "", created: role === undefined, rows };
  }

  // Modify, Create and Delete change nothing without View: ticking one ticks it.
  function tick(row: Row, permission: Permission, on: boolean) {
    const wanted = on ? [permission, "view"] : [];
    row.allow = PERMISSIONS.filter(
      (other) => wanted.includes(other) || (other !== permission && row.allow.includes(other)),
    );
  }

  // `create` is about groups: a host grant cannot have it.
  function setKind(row: Row, kind: string) {
    row.kind = kind === "host" ? "host" : "group";
    if (row.kind === "host") row.allow = row.allow.filter((other) => other !== "create");
  }

  async function saveRole(event: SubmitEvent) {
    event.preventDefault();
    if (!editing) return;
    const { name, created } = editing;
    const grant = editing.rows.map((row) => ({ [row.kind]: row.target, allow: row.allow }));
    const saved = await send(
      "role",
      () =>
        created
          ? api("roles", { method: "POST", body: { name, grant } })
          : api(path("roles", name), { method: "PUT", body: { grant } }),
      `Role ${name} saved.`,
    );
    if (saved) editing = undefined;
  }

  async function removeRole(role: Role) {
    if (!confirm(`Remove role ${role.name}?`)) return;
    await send(`role:${role.name}`, () => api(path("roles", role.name), { method: "DELETE" }), `Role ${role.name} removed.`);
  }

  onMount(() => loadWithToken(load));
</script>

{#snippet refusal(about: string, columns: number)}
  {#if refusals[about]}
    <tr>
      <td colspan={columns}><p class="failure" role="alert">{refusals[about]}</p></td>
    </tr>
  {/if}
{/snippet}

{#snippet passwords(state: { password: string; repeat: string }, label: string)}
  <label>
    {label}
    <input type="password" bind:value={state.password} autocomplete="new-password" minlength="8" required />
  </label>
  <label>
    Repeat the {label.toLowerCase()}
    <input type="password" bind:value={state.repeat} autocomplete="new-password" required />
  </label>
{/snippet}

{#if failure}
  <p class="failure" role="alert">{failure}</p>
{/if}
{#if notice}
  <p class="notice" role="status">{notice}</p>
{/if}

<section aria-labelledby="users-title">
  <h2 id="users-title">Users</h2>
  {#if adding}
    <form class="form" aria-labelledby="new-user-title" onsubmit={addUser}>
      <h3 id="new-user-title">New user</h3>
      <label>
        Name
        <input type="text" bind:value={adding.name} autocomplete="off" autocapitalize="none" required />
      </label>
      {@render passwords(adding, "Password")}
      <fieldset class="choices">
        <legend>Roles</legend>
        {#each roles as role (role.name)}
          <label>
            <input
              type="checkbox"
              checked={adding.roles.includes(role.name)}
              onchange={(event) => adding && (adding.roles = toggle(adding.roles, role.name, event.currentTarget.checked))}
            />
            {role.name}
          </label>
        {/each}
      </fieldset>
      {#if refusals["new-user"]}
        <p class="failure" role="alert">{refusals["new-user"]}</p>
      {/if}
      <div class="form-actions">
        <button type="submit" class="button">Add user</button>
        <button type="button" class="button" onclick={() => (adding = undefined)}>Cancel</button>
      </div>
    </form>
  {:else}
    <div class="toolbar">
      <button
        type="button"
        class="button"
        onclick={() => {
          delete refusals["new-user"];
          adding = { name: "", password: "", repeat: "", roles: [] };
        }}>New user</button
      >
    </div>
  {/if}

  {#if passwordFor}
    <form class="form" aria-labelledby="password-title" onsubmit={setPassword}>
      <h3 id="password-title">New password for {passwordFor.name}</h3>
      <p class="muted">
        Ends every session of {passwordFor.name}{passwordFor.name === me ? ", this one included" : ""}.
      </p>
      {@render passwords(passwordFor, "New password")}
      {#if refusals.password}
        <p class="failure" role="alert">{refusals.password}</p>
      {/if}
      <div class="form-actions">
        <button type="submit" class="button">Set password</button>
        <button type="button" class="button" onclick={() => (passwordFor = undefined)}>Cancel</button>
      </div>
    </form>
  {/if}

  <div class="scroll">
    <table class="list">
      <thead>
        <tr>
          <th scope="col">Name</th>
          <th scope="col">Roles</th>
          <th scope="col"><span class="visually-hidden">Actions</span></th>
        </tr>
      </thead>
      <tbody>
        {#each users as user (user.name)}
          {@const held = rolesOf(user)}
          <tr>
            <th scope="row">{user.name}</th>
            <td>
              <div class="choices">
                {#each roles as role (role.name)}
                  <label>
                    <input
                      type="checkbox"
                      checked={held.includes(role.name)}
                      onchange={(event) => (drafts[user.name] = toggle(held, role.name, event.currentTarget.checked))}
                    />
                    {role.name}
                  </label>
                {/each}
              </div>
            </td>
            <td class="actions">
              <button
                type="button"
                class="button"
                aria-label="Save roles of {user.name}"
                disabled={drafts[user.name] === undefined}
                onclick={() => saveRoles(user)}>Save</button
              >
              <button
                type="button"
                class="button"
                aria-label="Set password for {user.name}"
                onclick={() => {
                  delete refusals.password;
                  passwordFor = { name: user.name, password: "", repeat: "" };
                }}>Set password</button
              >
              <button
                type="button"
                class="button danger"
                aria-label="Remove {user.name}"
                onclick={() => removeUser(user)}>Remove</button
              >
            </td>
          </tr>
          {@render refusal(`user:${user.name}`, 3)}
        {/each}
        {#if users.length === 0}
          <tr><td colspan="3" class="muted">No user: the token and the machine are the way in.</td></tr>
        {/if}
      </tbody>
    </table>
  </div>
</section>

<section aria-labelledby="roles-title">
  <h2 id="roles-title">Roles</h2>
  {#if editing}
    <form class="form" aria-labelledby="role-title" onsubmit={saveRole}>
      <h3 id="role-title">{editing.created ? "New role" : `Role ${editing.name}`}</h3>
      {#if editing.created}
        <label>
          Role name
          <input type="text" bind:value={editing.name} autocomplete="off" autocapitalize="none" required />
        </label>
      {/if}
      <div class="scroll">
        <table class="list grants">
          <thead>
            <tr>
              <th scope="col">On</th>
              <th scope="col">Group or host name</th>
              {#each PERMISSIONS as permission (permission)}
                <th scope="col">{LABELS[permission]}</th>
              {/each}
              <th scope="col"><span class="visually-hidden">Actions</span></th>
            </tr>
          </thead>
          <tbody>
            {#each editing.rows as row, index (row)}
              {@const number = index + 1}
              <tr>
                <td>
                  <select
                    aria-label="Kind of grant {number}"
                    value={row.kind}
                    onchange={(event) => setKind(row, event.currentTarget.value)}
                  >
                    <option value="group">group</option>
                    <option value="host">host</option>
                  </select>
                </td>
                <td>
                  <input
                    type="text"
                    aria-label="Target of grant {number}"
                    list={row.kind === "group" ? "known-groups" : "known-hosts"}
                    bind:value={row.target}
                    autocomplete="off"
                    autocapitalize="none"
                    required
                  />
                </td>
                {#each PERMISSIONS as permission (permission)}
                  <td>
                    <input
                      type="checkbox"
                      aria-label="{LABELS[permission]} (grant {number})"
                      checked={row.allow.includes(permission)}
                      disabled={permission === "create" && row.kind === "host"}
                      onchange={(event) => tick(row, permission, event.currentTarget.checked)}
                    />
                  </td>
                {/each}
                <td class="actions">
                  <button
                    type="button"
                    class="button"
                    aria-label="Remove grant {number}"
                    onclick={() => editing?.rows.splice(index, 1)}>Remove</button
                  >
                </td>
              </tr>
            {/each}
            {#if editing.rows.length === 0}
              <tr><td colspan="7" class="muted">No grant: the role allows nothing.</td></tr>
            {/if}
          </tbody>
        </table>
      </div>
      <datalist id="known-groups">
        {#each groups as group (group)}<option value={group}></option>{/each}
      </datalist>
      <datalist id="known-hosts">
        {#each hosts as host (host)}<option value={host}></option>{/each}
      </datalist>
      {#if refusals.role}
        <p class="failure" role="alert">{refusals.role}</p>
      {/if}
      <div class="form-actions">
        <button
          type="button"
          class="button"
          onclick={() => editing?.rows.push({ kind: "group", target: "", allow: [] })}>Add grant</button
        >
        <button type="submit" class="button">Save role</button>
        <button type="button" class="button" onclick={() => (editing = undefined)}>Cancel</button>
      </div>
    </form>
  {:else}
    <div class="toolbar">
      <button type="button" class="button" onclick={() => edit()}>New role</button>
    </div>
    <div class="scroll">
      <table class="list">
        <thead>
          <tr>
            <th scope="col">Name</th>
            <th scope="col">Grants</th>
            <th scope="col"><span class="visually-hidden">Actions</span></th>
          </tr>
        </thead>
        <tbody>
          {#each roles as role (role.name)}
            <tr>
              <th scope="row">{role.name}</th>
              <td>
                {#if role.builtin}
                  <span class="badge">built in</span> every permission, on everything
                {:else}
                  {#each role.grant ?? [] as grant, index (index)}
                    <div>{summary(grant)}</div>
                  {:else}
                    <span class="muted">nothing</span>
                  {/each}
                {/if}
              </td>
              <td class="actions">
                {#if !role.builtin}
                  <button type="button" class="button" aria-label="Edit {role.name}" onclick={() => edit(role)}
                    >Edit</button
                  >
                  <button
                    type="button"
                    class="button danger"
                    aria-label="Remove {role.name}"
                    onclick={() => removeRole(role)}>Remove</button
                  >
                {/if}
              </td>
            </tr>
            {@render refusal(`role:${role.name}`, 3)}
          {/each}
        </tbody>
      </table>
    </div>
  {/if}
</section>

<style>
  section {
    margin-bottom: 1.5rem;
  }

  h2 {
    margin: 0 0 0.5rem;
    font-size: 1.1rem;
  }

  h3 {
    margin: 0;
    font-size: 1rem;
    overflow-wrap: anywhere;
  }

  .form {
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: 0.6rem;
    max-width: 40rem;
    margin-bottom: 1rem;
    padding: 0.75rem;
    border: 1px solid var(--border);
    border-radius: 6px;
  }

  .form > label {
    display: grid;
    gap: 0.2rem;
    max-width: 24rem;
  }

  .form p {
    margin: 0;
  }

  .form-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
  }

  fieldset {
    min-width: 0;
    margin: 0;
    padding: 0.25rem 0.75rem 0.5rem;
    border: 1px solid var(--border);
    border-radius: 6px;
  }

  input[type="text"],
  input[type="password"],
  select {
    width: 100%;
    padding: 0.3rem 0.45rem;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--background);
    color: inherit;
    font: inherit;
  }

  /* The checkboxes take their own lines in the table of a role. */
  .grants td {
    vertical-align: middle;
  }

  .grants input[type="text"] {
    min-width: 9rem;
  }

  .choices {
    margin: 0;
  }

  /* A role name stays on one line: the table scrolls instead. */
  .choices label {
    white-space: nowrap;
  }

  .grants select {
    min-width: 5rem;
  }

  td .failure {
    margin: 0;
  }

  .button:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
</style>
