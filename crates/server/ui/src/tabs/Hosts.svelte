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
  let failure = $state("");
  let notice = $state("");
  let editing: Entity | { kind: "server"; file: number } | undefined = $state.raw();
  let file = $state(0);

  const entities = $derived(files?.entities ?? []);
  const hosts = $derived(
    entities
      .filter((entity) => entity.kind === "server")
      .map((entity) => ({ entity, value: entity.value, live: liveOf(entity.value) })),
  );
  // Listeners no file describes: a provider or a `PATCH` made them.
  const runtime = $derived(servers.filter((server) => !hosts.some((host) => host.live === server)));

  // The `forward` control of the editor offers the pools of the files.
  setContext("pools", () =>
    entities.filter((entity) => entity.kind === "upstream").map((entity) => entity.key),
  );

  function list(value: unknown): string[] {
    return [value ?? []].flat().map(String);
  }

  /** The listener of a server: the one on its address, or the one that carries its name. */
  function liveOf(value: Record<string, unknown>): Live | undefined {
    const listens = list(value.listen);
    return servers.find(
      (server) =>
        listens.includes(server.listen) ||
        (typeof value.name === "string" && server.name?.split(", ").includes(value.name)),
    );
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
    // The revision is that of the list on screen: if the file changed since,
    // the key may be another host and the delete is refused.
    const revision = encodeURIComponent(fileOf(entity.file)?.revision ?? "");
    await attempt(() =>
      api(`config/entities/server/${encodeURIComponent(entity.key)}?revision=${revision}`, {
        method: "DELETE",
      }),
    );
    const refused = failure;
    await load();
    failure = refused;
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
          <th scope="col"><span class="hidden">Actions</span></th>
        </tr>
      </thead>
      <tbody>
        {#each hosts as { entity, value, live } (entity.key)}
          <tr>
            <th scope="row">{value.name ?? "—"}</th>
            <td>{list(value.host).join(", ") || "any"}</td>
            <td><code>{list(value.listen).join(", ")}</code></td>
            <td>{value.protocol ?? "http"}</td>
            <td>{target(value)}</td>
            <td>{tls(value.tls)}</td>
            <td class={["state", { bad: live?.state !== "listening" }]}>{live?.state ?? "not running"}</td>
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
        {#each runtime as server (server.listen)}
          <tr>
            <th scope="row">{server.name ?? "—"}</th>
            <td>—</td>
            <td><code>{server.listen}</code></td>
            <td>{server.protocol}</td>
            <td>—</td>
            <td>—</td>
            <td class={["state", { bad: server.state !== "listening" }]}>{server.state}</td>
            <td>
              <span class="badge" title="Not in a configuration file: made by a provider or a PATCH">
                runtime
              </span>
            </td>
            <td></td>
          </tr>
        {/each}
        {#if files && hosts.length + runtime.length === 0}
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

  .hidden {
    position: absolute;
    clip-path: inset(50%);
  }
</style>
