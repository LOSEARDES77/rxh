<script lang="ts">
  import { onMount } from "svelte";
  import { ApiError, api, loadWithToken, stopped } from "../lib/api";
  import EntityEditor from "../lib/EntityEditor.svelte";
  import type { Entity, Files, Schema } from "../lib/schema";

  // `acme` is edited in the certificates tab.
  const SECTIONS = [
    { key: "top", label: "General", table: "keys outside any table" },
    { key: "admin", label: "Admin API", table: "[admin]" },
    { key: "log", label: "Access log", table: "[log]" },
    { key: "tracing", label: "Tracing", table: "[tracing]" },
    { key: "docker", label: "Docker", table: "[docker]" },
    { key: "consul", label: "Consul", table: "[consul]" },
    { key: "etcd", label: "etcd", table: "[etcd]" },
    { key: "nomad", label: "Nomad", table: "[nomad]" },
    { key: "kubernetes", label: "Kubernetes", table: "[kubernetes]" },
  ];

  const ACTIONS = {
    upgrade: {
      label: "Upgrade",
      effect:
        "Replaces the running process with the rxh binary on disk. The new process takes the listeners over; Linux only.",
    },
    stop: {
      label: "Stop",
      effect:
        "Ends the daemon: the proxy stops serving every host, and this page loses its API. Only the machine can start it again.",
    },
  };

  let files: Files | undefined = $state.raw();
  let schema: Schema | undefined = $state.raw();
  // Of the last read; a later read that works clears it.
  let failure = $state("");
  // Of the last change asked for: it stays until the next one.
  let refused = $state("");
  let notice = $state("");
  let busy = $state(false);
  let editing: Entity | { kind: "section"; file: number; key: string } | undefined = $state.raw();
  // The text typed in each file, by index.
  let drafts: Record<number, string> = $state({});
  // The text and revision of the disk each draft was read from: a save is
  // made over that revision, so a change made since is refused, never lost.
  let bases: Record<number, { text: string; revision: string }> = $state({});
  // What a file held when it was read again after a conflict, by index.
  let holds: Record<number, string> = $state({});
  let refusals: Record<number, { status: number; message: string }> = $state({});
  let asking: "upgrade" | "stop" | undefined = $state();
  let word = $state("");
  let dialog: HTMLDialogElement;

  const sections = $derived(
    SECTIONS.map((section) => ({
      ...section,
      entity: files?.entities.find((entity) => entity.kind === "section" && entity.key === section.key),
    })),
  );

  function fileOf(index: number) {
    return files?.files.find((other) => other.index === index);
  }

  function message(err: unknown): string {
    return err instanceof Error ? err.message : String(err);
  }

  /**
   * Reads the files. A draft nobody changed follows the disk, as does the
   * file just `saved`; a changed draft is kept on its old revision, except
   * for the file `accepted` ("Read again"), whose draft moves to the new one.
   */
  async function load(saved?: number, accepted?: number) {
    try {
      [files, schema] = await Promise.all([api<Files>("config/files"), schema ?? api<Schema>("schema")]);
      for (const file of files.files) {
        const base = bases[file.index];
        // This tab is an administrator's, who gets the text of every file.
        const disk = { text: file.text ?? "", revision: file.revision };
        if (!base || file.index === saved || drafts[file.index] === base.text) {
          drafts[file.index] = disk.text;
          bases[file.index] = disk;
          delete holds[file.index];
          delete refusals[file.index];
        } else if (file.index === accepted) {
          bases[file.index] = disk;
          holds[file.index] = disk.text;
          delete refusals[file.index];
        }
      }
      failure = "";
    } catch (err) {
      failure = message(err);
    }
  }

  // A typed draft whose file changed on disk since it was read.
  function stale(index: number): boolean {
    const base = bases[index];
    return !!base && drafts[index] !== base.text && base.revision !== fileOf(index)?.revision;
  }

  async function saveFile(index: number) {
    delete refusals[index];
    notice = "";
    try {
      await api(`config/files/${index}`, {
        method: "PUT",
        body: { text: drafts[index], revision: bases[index]?.revision ?? "" },
      });
      notice = `${fileOf(index)?.path} saved and reloaded.`;
      await load(index);
    } catch (err) {
      // The text stays as typed.
      refusals[index] = { status: err instanceof ApiError ? err.status : 0, message: message(err) };
    }
  }

  // The typed text stays; the next save is made over what the file holds now.
  function readAgain(index: number) {
    return load(undefined, index);
  }

  async function control(path: string, done: string) {
    busy = true;
    refused = "";
    notice = "";
    try {
      await api(path, { method: "POST" });
      notice = done;
      return true;
    } catch (err) {
      refused = message(err);
      return false;
    } finally {
      busy = false;
    }
  }

  async function reload() {
    if (!confirm("Read the configuration files again and apply them?")) return;
    if (await control("reload", "Reloaded: the configuration files were read again.")) await load();
  }

  function ask(action: "upgrade" | "stop") {
    asking = action;
    word = "";
    dialog.showModal();
  }

  async function act(event: SubmitEvent) {
    event.preventDefault();
    const action = asking;
    if (!action || word !== action) return;
    dialog.close();
    if (action === "upgrade") {
      await control("upgrade", "Upgraded: the new process took the listeners over.");
    } else if (await control("stop", "")) {
      stopped.set(true);
    }
  }

  function close() {
    editing = undefined;
    load();
  }

  onMount(() => loadWithToken(load));
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
  {#if editing.key === "admin"}
    <p class="notice">
      Changing the address or the token takes effect on restart and can lock this page out.
    </p>
  {/if}
  {#key editing}
    <EntityEditor
      {schema}
      entity={editing}
      revision={fileOf(editing.file)?.revision ?? ""}
      onsaved={close}
      oncancel={() => (editing = undefined)}
      onreread={() => load()}
    />
  {/key}
{:else}
  {#if files && !files.editable}
    <p class="notice">No configuration file: read only</p>
  {/if}

  <section aria-labelledby="sections-title">
    <h2 id="sections-title">Sections</h2>
    <div class="scroll">
      <table class="list sections">
        <thead>
          <tr>
            <th scope="col">Section</th>
            <th scope="col">Table</th>
            <th scope="col">In file</th>
            <th scope="col"><span class="visually-hidden">Actions</span></th>
          </tr>
        </thead>
        <tbody>
          {#each sections as section (section.key)}
            <tr>
              <th scope="row">{section.label}</th>
              <td class="muted">
                {#if section.key === "top"}{section.table}{:else}<code>{section.table}</code>{/if}
              </td>
              <td>
                {#if section.entity}
                  {@const path = fileOf(section.entity.file)?.path ?? ""}
                  <span title={path}>{path.split("/").pop()}</span>
                {:else}
                  <span class="muted">not set</span>
                {/if}
              </td>
              <td class="actions">
                {#if files?.editable}
                  {@const entity = section.entity}
                  {@const file = files.files[0]?.index ?? 0}
                  <button
                    type="button"
                    class="button"
                    onclick={() => (editing = entity ?? { kind: "section", file, key: section.key })}
                  >
                    {entity ? "Edit" : "Set up"} {section.label}
                  </button>
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  </section>

  {#if files?.editable}
    <section aria-labelledby="files-title">
      <h2 id="files-title">Files</h2>
      <p class="muted">
        Secrets show as <code>***</code>; leave them so to keep what is stored.
      </p>
      {#each files.files as file (file.index)}
        {@const refusal = refusals[file.index]}
        <div class="file">
          <label for="file-{file.index}"><code>{file.path}</code></label>
          <textarea id="file-{file.index}" rows="16" spellcheck="false" bind:value={drafts[file.index]}
          ></textarea>
          {#if !refusal && stale(file.index)}
            <div class="failure" role="alert">
              <p>The file changed on disk after this text was typed: saving it now is refused.</p>
              <p>Reading again keeps what is typed here and shows what the file holds now.</p>
              <button type="button" class="button" onclick={() => readAgain(file.index)}>Read again</button>
            </div>
          {/if}
          {#if refusal}
            <div class="failure" role="alert">
              <p>{refusal.message}</p>
              {#if refusal.status === 409}
                <p>Reading again keeps what is typed here and shows what the file holds now.</p>
                <button type="button" class="button" onclick={() => readAgain(file.index)}>Read again</button>
              {/if}
            </div>
          {/if}
          {#if holds[file.index] !== undefined}
            <label for="holds-{file.index}">What <code>{file.path}</code> holds now</label>
            <textarea id="holds-{file.index}" rows="8" readonly value={holds[file.index]}></textarea>
          {/if}
          <div>
            <button
              type="button"
              class="button"
              aria-label="Save and reload {file.path}"
              onclick={() => saveFile(file.index)}
            >
              Save and reload
            </button>
          </div>
        </div>
      {/each}
    </section>
  {/if}

  <section aria-labelledby="daemon-title">
    <h2 id="daemon-title">Daemon</h2>
    <dl class="daemon">
      <dt><button type="button" class="button" disabled={busy} onclick={reload}>Reload</button></dt>
      <dd>Reads the configuration files again and applies them, as a change of the files would.</dd>
      <dt>
        <button type="button" class="button danger" disabled={busy} onclick={() => ask("upgrade")}>
          Upgrade…
        </button>
      </dt>
      <dd>{ACTIONS.upgrade.effect}</dd>
      <dt>
        <button type="button" class="button danger" disabled={busy} onclick={() => ask("stop")}>Stop…</button>
      </dt>
      <dd>{ACTIONS.stop.effect}</dd>
    </dl>
  </section>
{/if}

<!-- Escape closes it like Cancel: `close` forgets the word. -->
<dialog bind:this={dialog} aria-labelledby="confirm-title" onclose={() => (word = "")}>
  {#if asking}
    {@const action = ACTIONS[asking]}
    <form onsubmit={act}>
      <h2 id="confirm-title">{action.label} rxh?</h2>
      <p>{action.effect}</p>
      <label for="confirm-word">Type <strong>{asking}</strong> to confirm</label>
      <input id="confirm-word" type="text" autocomplete="off" spellcheck="false" bind:value={word} />
      <div class="dialog-actions">
        <button type="button" class="button" onclick={() => dialog.close()}>Cancel</button>
        <button type="submit" class="button danger" disabled={word !== asking}>{action.label} rxh</button>
      </div>
    </form>
  {/if}
</dialog>

<style>
  section {
    margin-bottom: 1.5rem;
  }

  h2 {
    margin: 0 0 0.5rem;
    font-size: 1.1rem;
  }

  /* The buttons are taller than the text of the row. */
  .sections th,
  .sections td {
    vertical-align: middle;
  }

  .file {
    display: grid;
    /* Not wider than the page, however long the path above the text is. */
    grid-template-columns: minmax(0, 1fr);
    overflow-wrap: anywhere;
    gap: 0.3rem;
    margin-bottom: 1rem;
  }

  .file .failure {
    margin: 0;
    /* Only the message keeps its line breaks, not the markup around it. */
    white-space: normal;
  }

  .file .failure p {
    margin: 0 0 0.25rem;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  textarea,
  input {
    width: 100%;
    padding: 0.3rem 0.45rem;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--background);
    color: inherit;
    font: inherit;
  }

  textarea {
    font-family: ui-monospace, monospace;
  }

  textarea[readonly] {
    background: var(--surface);
  }

  .daemon {
    display: grid;
    grid-template-columns: auto 1fr;
    gap: 0.5rem 1rem;
    align-items: baseline;
    margin: 0;
  }

  .daemon dd {
    margin: 0;
  }

  dialog {
    max-width: min(30rem, calc(100vw - 2rem));
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--background);
    color: var(--text);
  }

  dialog::backdrop {
    background: rgb(0 0 0 / 40%);
  }

  dialog form {
    display: grid;
    gap: 0.6rem;
  }

  dialog p {
    margin: 0;
  }

  .dialog-actions {
    display: flex;
    justify-content: flex-end;
    gap: 0.5rem;
  }

  .button:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
</style>
