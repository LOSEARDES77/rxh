<script lang="ts">
  import { onMount, setContext } from "svelte";
  import { api } from "../lib/api";
  import EntityEditor from "../lib/EntityEditor.svelte";
  import { poll } from "../lib/poll";
  import { isTable, type Entity, type Files, type Schema } from "../lib/schema";

  type Live = { listen: string; protocol: string; name: string | null; state: string };

  let files: Files | undefined = $state.raw();
  let schema: Schema | undefined = $state.raw();
  let servers: Live[] = $state.raw([]);
  // Of the last read; a later read that works clears it.
  let failure = $state("");
  // Of the last change asked for: it stays until the next one.
  let refused = $state("");
  let notice = $state("");
  let editing: Entity | { kind: "server"; file: number } | undefined = $state.raw();
  let file = $state(0);

  const entities = $derived(files?.entities ?? []);
  const hosts = $derived(
    entities
      .filter((entity) => entity.kind === "server")
      .map((entity) => ({
        entity,
        value: entity.value,
        listeners: listenersOf(entity.value),
        open: isOpen(entity.value),
      })),
  );
  // Listeners no server of the files accounts for.
  const others = $derived(
    servers.filter((server) => !hosts.some((host) => host.listeners.includes(server))),
  );
  // While a server of the files cannot be told from its listen address, a
  // listener without a name may be its own: it is not called "runtime".
  const unsure = $derived(hosts.some((host) => host.open && host.listeners.length === 0));

  // The `forward` control of the editor offers the pools of the files.
  setContext("pools", () =>
    entities.filter((entity) => entity.kind === "upstream").map((entity) => entity.key),
  );

  function list(value: unknown): string[] {
    return [value ?? []].flat().map(String);
  }

  function named(value: Record<string, unknown>): string | undefined {
    return typeof value.name === "string" && !value.name.includes("{") ? value.name : undefined;
  }

  /**
   * The listeners of a server, one per socket: those that carry its name,
   * and for a server without a name those on its listen addresses.
   */
  function listenersOf(value: Record<string, unknown>): Live[] {
    const name = named(value);
    const listens = list(value.listen);
    const byName = servers.filter((server) => name && server.name?.split(", ").includes(name));
    return byName.length > 0 ? byName : servers.filter((server) => listens.includes(server.listen));
  }

  /** No name, and a listen address the file does not spell out: port 0 or a placeholder. */
  function isOpen(value: Record<string, unknown>): boolean {
    const vague = list(value.listen).some((listen) => listen.endsWith(":0") || listen.includes("{"));
    return vague && named(value) === undefined;
  }

  function stateOf(host: { listeners: Live[]; open: boolean }): string {
    const states = [...new Set(host.listeners.map((listener) => listener.state))];
    return states.join(", ") || (host.open ? "unknown" : "not running");
  }

  function title(value: Record<string, unknown>): string {
    return String(value.name ?? list(value.listen).join(", "));
  }

  function target(value: Record<string, unknown>): string {
    const { forward, serve, redirect } = value;
    if (typeof forward === "string") return forward;
    if (Array.isArray(forward)) {
      return forward.map((item) => (isTable(item) ? (item.pool ?? item.address) : item)).join(", ");
    }
    if (forward !== undefined) return "inline pool";
    if (serve !== undefined) return `files of ${isTable(serve) ? serve.root : serve}`;
    if (redirect !== undefined) return `redirect to ${isTable(redirect) ? redirect.to : redirect}`;
    return Array.isArray(value.match) ? `${value.match.length} routes` : "—";
  }

  function tls(value: unknown): string {
    if (value === undefined) return "off";
    if (value === "auto") return "automatic";
    return isTable(value) ? "certificate files" : String(value);
  }

  function fileOf(index: number) {
    return files?.files.find((other) => other.index === index);
  }

  async function attempt(work: () => Promise<unknown>) {
    try {
      await work();
      failure = "";
    } catch (err) {
      failure = err instanceof Error ? err.message : String(err);
    }
  }

  function refresh() {
    return attempt(async () => (servers = (await api<{ servers: Live[] }>("status")).servers));
  }

  function load() {
    return attempt(async () => {
      [files, schema] = await Promise.all([api<Files>("config/files"), schema ?? api<Schema>("schema")]);
    });
  }

  function open(next: typeof editing) {
    notice = "";
    refused = "";
    editing = next;
  }

  function close(message = "") {
    notice = message;
    editing = undefined;
    load();
    refresh();
  }

  async function remove(entity: Entity) {
    const path = fileOf(entity.file)?.path;
    if (!confirm(`Delete host ${title(entity.value)} from ${path}?`)) return;
    notice = "";
    refused = "";
    // The revision is that of the list on screen: if the file changed since,
    // the key may be another host and the delete is refused.
    const revision = encodeURIComponent(fileOf(entity.file)?.revision ?? "");
    try {
      await api(`config/entities/server/${encodeURIComponent(entity.key)}?revision=${revision}`, {
        method: "DELETE",
      });
    } catch (err) {
      refused = err instanceof Error ? err.message : String(err);
    }
    load();
    refresh();
  }

  onMount(() => {
    load();
    return poll(refresh);
  });
</script>

<!-- The name of the file is enough at a glance; its path is one hover away. -->
{#snippet source(path: string)}
  <td class="muted" title={path}>{path.split("/").pop()}</td>
{/snippet}

{#if failure}
  <p class="failure" role="alert">{failure}</p>
{/if}
{#if refused}
  <p class="failure" role="alert">{refused}</p>
{/if}
{#if notice}
  <p class="notice" role="status">{notice}</p>
{/if}

{#if editing && schema}
  {#key editing}
    <EntityEditor
      {schema}
      entity={editing}
      revision={fileOf(editing.file)?.revision ?? ""}
      onsaved={() => close()}
      oncancel={() => (editing = undefined)}
      onreread={(moved) => (moved ? close(moved) : load())}
    />
  {/key}
{:else}
  {#if files?.editable}
    <div class="toolbar">
      <button type="button" class="button" onclick={() => open({ kind: "server", file })}>
        New host
      </button>
      {#if files.files.length > 1}
        <label for="host-file">in file</label>
        <select id="host-file" aria-label="File of a new host" bind:value={file}>
          {#each files.files as other (other.index)}
            <option value={other.index}>{other.path}</option>
          {/each}
        </select>
      {/if}
    </div>
  {:else if files}
    <p class="notice">No configuration file: read only</p>
  {/if}

  <div class="scroll">
    <table class="list">
      <thead>
        <tr>
          <th scope="col">Name</th>
          <th scope="col">Host names</th>
          <th scope="col">Listen</th>
          <th scope="col">Protocol</th>
          <th scope="col">Target</th>
          <th scope="col">TLS</th>
          <th scope="col">State</th>
          <th scope="col">Source</th>
          <th scope="col"><span class="visually-hidden">Actions</span></th>
        </tr>
      </thead>
      <tbody>
        {#each hosts as host (host.entity.key)}
          {@const { entity, value } = host}
          {@const text = stateOf(host)}
          <tr>
            <th scope="row">{value.name ?? "—"}</th>
            <td>{list(value.host).join(", ") || "any"}</td>
            <td><code>{list(value.listen).join(", ")}</code></td>
            <td>{value.protocol ?? "http"}</td>
            <td>{target(value)}</td>
            <td>{tls(value.tls)}</td>
            <td
              class={["state", { bad: text !== "listening" }]}
              title={text === "unknown"
                ? "The listen address of the file is a placeholder or port 0: its listener cannot be told apart"
                : undefined}
            >
              {text}
            </td>
            {@render source(fileOf(entity.file)?.path ?? "")}
            <td class="actions">
              <button
                type="button"
                class="button"
                aria-label="Edit {title(value)}"
                onclick={() => open(entity)}
              >
                Edit
              </button>
              <button
                type="button"
                class="button danger"
                aria-label="Delete {title(value)}"
                onclick={() => remove(entity)}
              >
                Delete
              </button>
            </td>
          </tr>
        {/each}
        {#each others as server (server.listen)}
          <tr>
            <th scope="row">{server.name ?? "—"}</th>
            <td>—</td>
            <td><code>{server.listen}</code></td>
            <td>{server.protocol}</td>
            <td>—</td>
            <td>—</td>
            <td class={["state", { bad: server.state !== "listening" }]}>{server.state}</td>
            <td>
              {#if unsure && server.name === null}
                <span
                  class="badge"
                  title="May be a server of the files whose listen address is a placeholder or port 0"
                >
                  not matched
                </span>
              {:else}
                <span class="badge" title="Not in a configuration file: made by a provider or a PATCH">
                  runtime
                </span>
              {/if}
            </td>
            <td></td>
          </tr>
        {/each}
        {#if files && hosts.length + others.length === 0}
          <tr><td colspan="9" class="muted">No host.</td></tr>
        {/if}
      </tbody>
    </table>
  </div>
{/if}

<style>
  code,
  .state {
    white-space: nowrap;
  }

  .actions {
    text-align: right;
  }
</style>
