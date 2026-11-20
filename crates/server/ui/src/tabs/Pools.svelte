<script lang="ts">
  import { onMount } from "svelte";
  import { api, loadWithToken } from "../lib/api";
  import EntityEditor from "../lib/EntityEditor.svelte";
  import { poll } from "../lib/poll";
  import type { Entity, Files, Schema } from "../lib/schema";

  type Backend = {
    address: string;
    weight: number;
    healthy: boolean;
    available: boolean;
    num_requests: number;
    total_requests: number;
    fails: number;
    circuit?: string;
    state: string;
  };
  type Pool = { name: string; policy: unknown; queued: number; upstreams: Backend[] };

  const STATES = [
    { state: "ready", text: "Ready", effect: "takes new requests" },
    { state: "drain", text: "Drain", effect: "takes no new requests, except those of its sticky sessions" },
    { state: "maintenance", text: "Maintenance", effect: "takes no new requests" },
  ];

  let files: Files | undefined = $state.raw();
  let schema: Schema | undefined = $state.raw();
  let pools: Pool[] = $state.raw([]);
  // Of the last read; a later read that works clears it.
  let failure = $state("");
  // Of the last change asked for: it stays until the next one.
  let refused = $state("");
  let editing: Entity | { kind: "upstream"; file: number } | undefined = $state.raw();
  let file = $state(0);

  const entities = $derived((files?.entities ?? []).filter((entity) => entity.kind === "upstream"));
  // The running pools first, then those of the files that are not running.
  const cards = $derived([
    ...pools.map((pool) => ({
      name: pool.name,
      pool,
      entity: entities.find((entity) => entity.key === pool.name),
    })),
    ...entities
      .filter((entity) => !pools.some((pool) => pool.name === entity.key))
      .map((entity) => ({ name: entity.key, pool: undefined, entity })),
  ]);

  function fileOf(index: number) {
    return files?.files.find((other) => other.index === index);
  }

  function health(backend: Backend): string {
    if (!backend.healthy) return "down";
    if (backend.circuit && backend.circuit !== "closed") return `circuit ${backend.circuit}`;
    // A drained backend is not available either: its state says so already.
    return backend.available || backend.state !== "ready" ? "healthy" : "healthy, at its limit";
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
    return attempt(async () => (pools = await api<Pool[]>("pools")));
  }

  function load() {
    return attempt(async () => {
      [files, schema] = await Promise.all([api<Files>("config/files"), schema ?? api<Schema>("schema")]);
    });
  }

  async function setState(pool: string, address: string, state: string) {
    refused = "";
    try {
      await api(`pools/${encodeURIComponent(pool)}/upstreams/state`, {
        method: "PUT",
        body: { address, state },
      });
    } catch (err) {
      refused = err instanceof Error ? err.message : String(err);
    }
    refresh();
  }

  function close() {
    editing = undefined;
    load();
    refresh();
  }

  onMount(() => {
    const forget = loadWithToken(load);
    const stop = poll(refresh);
    return () => {
      forget();
      stop();
    };
  });
</script>

{#if failure}
  <p class="failure" role="alert">{failure}</p>
{/if}
{#if refused}
  <p class="failure" role="alert">{refused}</p>
{/if}

{#if editing && schema}
  {#key editing}
    <EntityEditor
      {schema}
      entity={editing}
      revision={fileOf(editing.file)?.revision ?? ""}
      onsaved={close}
      oncancel={() => (editing = undefined)}
      onreread={load}
    />
  {/key}
{:else}
  {#if files?.editable}
    <div class="toolbar">
      <button type="button" class="button" onclick={() => (editing = { kind: "upstream", file })}>
        New pool
      </button>
      {#if files.files.length > 1}
        <label for="pool-file">in file</label>
        <select id="pool-file" aria-label="File of a new pool" bind:value={file}>
          {#each files.files as other (other.index)}
            <option value={other.index}>{other.path}</option>
          {/each}
        </select>
      {/if}
    </div>
  {:else if files}
    <p class="notice">No configuration file: read only</p>
  {/if}

  {#each cards as card (card.name)}
    <section class="card" aria-label="Pool {card.name}">
      <header>
        <h2>{card.name}</h2>
        {#if card.entity}
          {@const path = fileOf(card.entity.file)?.path ?? ""}
          <span class="muted" title={path}>{path.split("/").pop()}</span>
        {:else}
          <span class="badge" title="Not in a configuration file: inline, or made by a provider">runtime</span>
        {/if}
        {#if card.pool}
          <span>policy <code>{typeof card.pool.policy === "string" ? card.pool.policy : JSON.stringify(card.pool.policy)}</code></span>
          {#if card.pool.queued > 0}<span>{card.pool.queued} queued</span>{/if}
        {:else}
          <span class="badge">not running</span>
        {/if}
        {#if files?.editable && card.entity}
          {@const entity = card.entity}
          <button type="button" class="button" aria-label="Edit {card.name}" onclick={() => (editing = entity)}>
            Edit
          </button>
        {/if}
      </header>
      {#if card.pool}
        {@const pool = card.pool}
        <div class="scroll">
          <table class="list">
            <thead>
              <tr>
                <th scope="col">Backend</th>
                <th scope="col">Health</th>
                <th scope="col">State</th>
                <th scope="col" class="number">Weight</th>
                <th scope="col" class="number">In flight</th>
                <th scope="col" class="number">Requests</th>
                <th scope="col" class="number">Fails</th>
                <th scope="col">Set state (until restart)</th>
              </tr>
            </thead>
            <tbody>
              {#each pool.upstreams as backend (backend.address)}
                <tr>
                  <th scope="row"><code>{backend.address}</code></th>
                  <td class={{ bad: !backend.healthy }}>{health(backend)}</td>
                  <td class={{ bad: backend.state !== "ready" }}>{backend.state}</td>
                  <td class="number">{backend.weight}</td>
                  <td class="number">{backend.num_requests}</td>
                  <td class="number">{backend.total_requests}</td>
                  <td class="number">{backend.fails}</td>
                  <td class="actions">
                    {#each STATES as choice (choice.state)}
                      <button
                        type="button"
                        class="button"
                        aria-label="{choice.text} {backend.address} until restart"
                        aria-pressed={backend.state === choice.state}
                        title="{choice.text}: {choice.effect}. Lasts until rxh restarts; the file is not changed."
                        onclick={() => setState(pool.name, backend.address, choice.state)}
                      >
                        {choice.text}
                      </button>
                    {/each}
                  </td>
                </tr>
              {:else}
                <tr><td colspan="8" class="muted">No backend.</td></tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    </section>
  {:else}
    {#if files}<p class="muted">No pool.</p>{/if}
  {/each}
{/if}

<style>
  .card {
    margin-bottom: 1rem;
    padding: 0.75rem 1rem;
    border: 1px solid var(--border);
    border-radius: 6px;
  }

  header {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 0.25rem 1rem;
    margin-bottom: 0.25rem;
  }

  h2 {
    margin: 0;
    font-size: 1.1rem;
    overflow-wrap: anywhere;
  }

  header .button {
    margin-left: auto;
  }

  .muted {
    overflow-wrap: anywhere;
  }
</style>
