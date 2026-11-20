<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "../lib/api";
  import EntityEditor from "../lib/EntityEditor.svelte";
  import { poll } from "../lib/poll";
  import type { Entity, Files, Schema } from "../lib/schema";

  type Certificate = {
    listen: string;
    subject: string;
    issuer: string;
    names: string[];
    not_after: string;
    days_left: number;
    source: string;
    ocsp_stapled: boolean;
  };

  let files: Files | undefined = $state.raw();
  let schema: Schema | undefined = $state.raw();
  let certificates: Certificate[] = $state.raw([]);
  let read = $state(false);
  // Of the last read; a later read that works clears it.
  let failure = $state("");
  // Of the last change asked for: it stays until the next one.
  let refused = $state("");
  let notice = $state("");
  let editing: Entity | { kind: "section"; file: number; key: string } | undefined = $state.raw();

  const sorted = $derived([...certificates].sort((a, b) => a.days_left - b.days_left));
  const acme = $derived(
    files?.entities.find((entity) => entity.kind === "section" && entity.key === "acme"),
  );

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
    return attempt(async () => {
      certificates = await api<Certificate[]>("certificates");
      read = true;
    });
  }

  function load() {
    return attempt(async () => {
      [files, schema] = await Promise.all([api<Files>("config/files"), schema ?? api<Schema>("schema")]);
    });
  }

  async function reload() {
    refused = "";
    notice = "";
    try {
      const { reloaded } = await api<{ reloaded: string[] }>("certificates/reload", { method: "POST" });
      notice = `${reloaded.length} certificate file${reloaded.length === 1 ? "" : "s"} read again.`;
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
    load();
    return poll(refresh);
  });
</script>

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
      onsaved={close}
      oncancel={() => (editing = undefined)}
      onreread={load}
    />
  {/key}
{:else}
  <div class="toolbar">
    <button type="button" class="button" onclick={reload}>Reload certificates</button>
    <span class="muted">Reads every certificate and key file again, changed or not.</span>
  </div>

  {#if sorted.length > 0}
    <div class="scroll">
      <table class="list">
        <thead>
          <tr>
            <th scope="col">Names</th>
            <th scope="col">Listener</th>
            <th scope="col">Source</th>
            <th scope="col">Issuer</th>
            <th scope="col">Expires</th>
            <th scope="col" class="number">Days left</th>
            <th scope="col">OCSP</th>
          </tr>
        </thead>
        <tbody>
          {#each sorted as certificate, index (index)}
            <tr>
              <th scope="row">{certificate.names.join(", ") || certificate.subject}</th>
              <td><code>{certificate.listen}</code></td>
              <td>{certificate.source}</td>
              <td>{certificate.issuer}</td>
              <td title={certificate.not_after}>{certificate.not_after.slice(0, 10)}</td>
              <td class="number">
                {certificate.days_left}
                {#if certificate.days_left < 0}
                  <strong class="bad">expired</strong>
                {:else if certificate.days_left < 30}
                  <strong class="bad">expires soon</strong>
                {/if}
              </td>
              <td>{certificate.ocsp_stapled ? "stapled" : "not stapled"}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {:else if read}
    <p class="muted">No TLS certificate is served.</p>
  {/if}

  <section class="card" aria-labelledby="acme-title">
    <header>
      <h2 id="acme-title">ACME</h2>
      <code class="muted">[acme]</code>
      {#if files}
        <span class="badge">{acme ? fileOf(acme.file)?.path.split("/").pop() : "not set"}</span>
      {/if}
      {#if files?.editable}
        <button
          type="button"
          class="button"
          onclick={() => (editing = acme ?? { kind: "section", file: files?.files[0]?.index ?? 0, key: "acme" })}
        >
          {acme ? "Edit ACME" : "Set up ACME"}
        </button>
      {/if}
    </header>
    <p class="muted">Account, directory and challenges for the certificates rxh obtains itself.</p>
    {#if files && !files.editable}
      <p class="notice">No configuration file: read only</p>
    {/if}
  </section>
{/if}

<style>
  .card {
    margin-top: 1.5rem;
    padding: 0.75rem 1rem;
    border: 1px solid var(--border);
    border-radius: 6px;
  }

  header {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 0.25rem 1rem;
  }

  h2 {
    margin: 0;
    font-size: 1.1rem;
  }

  header .button {
    margin-left: auto;
  }

  .card p {
    margin: 0.5rem 0 0;
  }
</style>
