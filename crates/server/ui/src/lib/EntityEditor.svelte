<script lang="ts">
  import { untrack } from "svelte";
  import { ApiError, api } from "./api";
  import Action, { ACTIONS } from "./controls/Action.svelte";
  import { arrange, helpOf, human } from "./layout";
  import SchemaField from "./SchemaField.svelte";
  import {
    control,
    entitySchema,
    prune,
    type Entity,
    type Files,
    type Kind,
    type Schema,
  } from "./schema";

  let {
    schema,
    // One entity of `GET config/files`, or where to write a new one (with
    // `key`, its name is fixed and not asked for). The editor reads it once:
    // draw it under `{#key}` to edit another entity.
    entity,
    // Of the file of the entity: `files[entity.file].revision`.
    revision,
    onsaved,
    oncancel,
    // After "Read again": the list is out of date too. With a message, the
    // entity is no longer the one being edited and the editor must be closed.
    onreread = undefined,
  }: {
    schema: Schema;
    entity: Entity | { kind: Kind; file: number; key?: string };
    revision: string;
    onsaved: () => void;
    oncancel: () => void;
    onreread?: (moved?: string) => void;
  } = $props();

  const id = $props.id();
  const { kind, file } = untrack(() => entity);
  const key = untrack(() => ("value" in entity ? entity.key : undefined));
  // The name a new entity was given, when it may not be changed.
  const named = untrack(() => ("value" in entity ? undefined : entity.key));

  // What the file holds, to tell edits from it and to go back to it.
  let stored = $state.raw(
    untrack(() => ({
      value: "value" in entity ? entity.value : {},
      toml: "toml" in entity ? entity.toml : "",
      revision,
    })),
  );
  let name = $state(untrack(() => entity.key ?? ""));
  let mode: "form" | "toml" = $state("form");
  // The fields never change a value in place, so the stored table is safe to start from.
  let draft = $state.raw(untrack(() => stored.value));
  let toml = $state(untrack(() => stored.toml));
  let generation = $state(0);
  let failure: { status: number; message: string } | undefined = $state();
  let busy = $state(false);

  const noun = { server: "server", upstream: "pool", section: "section" }[kind];
  // The key of a server is its position, which says nothing to a person.
  const shown = untrack(() =>
    kind === "server" && "value" in entity
      ? String(entity.value.name ?? [entity.value.listen ?? key].flat().join(", "))
      : key,
  );
  const title =
    key === undefined
      ? `New ${noun}${named === undefined ? "" : ` ${named}`}`
      : `${noun.charAt(0).toUpperCase()}${noun.slice(1)} ${shown}`;

  const shape = $derived(control(schema, entitySchema(schema, kind, key ?? name)));
  const fields = $derived(shape.kind === "table" ? shape.fields : {});
  // The keys of the file the schema does not know (aliases) are drawn too.
  const groups = $derived(
    arrange(kind, [
      ...Object.keys(fields),
      ...Object.keys(stored.value).filter((known) => !(known in fields)),
    ]),
  );
  const dirty = $derived(
    mode === "form"
      ? JSON.stringify(prune(draft)) !== JSON.stringify(prune(stored.value))
      : toml !== stored.toml,
  );

  function setKey(field: string, next: unknown) {
    const copy = { ...draft };
    if (next === undefined) {
      delete copy[field];
    } else {
      copy[field] = next;
    }
    draft = copy;
  }

  function reset() {
    draft = stored.value;
    toml = stored.toml;
    generation += 1;
  }

  // The two views are not converted into each other: switching starts again from the file.
  function switchTo(event: MouseEvent, next: "form" | "toml") {
    if (next === mode) return;
    if (dirty && !confirm("Discard the unsaved changes?")) {
      event.preventDefault();
      return;
    }
    reset();
    mode = next;
  }

  function fail(err: unknown) {
    failure = {
      status: err instanceof ApiError ? err.status : 0,
      message: err instanceof Error ? err.message : String(err),
    };
  }

  async function send(request: () => Promise<unknown>) {
    busy = true;
    failure = undefined;
    try {
      await request();
      onsaved();
    } catch (err) {
      // The inputs keep their contents: nothing typed is lost to a refusal.
      fail(err);
    } finally {
      busy = false;
    }
  }

  function save() {
    // The whole table goes back, with the keys no field draws: a key left out is removed.
    const content = mode === "form" ? { value: prune(draft) } : { toml };
    return send(() =>
      key === undefined
        ? api(`config/entities/${kind}`, {
            method: "POST",
            body: { ...content, ...(name === "" ? {} : { key: name }), file },
          })
        : api(`config/entities/${kind}/${encodeURIComponent(key)}`, {
            method: "PUT",
            body: { ...content, revision: stored.revision },
          }),
    );
  }

  function remove() {
    if (key === undefined || !confirm(`Delete ${noun} ${shown} from the configuration?`)) return;
    const query = `revision=${encodeURIComponent(stored.revision)}`;
    return send(() =>
      api(`config/entities/${kind}/${encodeURIComponent(key)}?${query}`, { method: "DELETE" }),
    );
  }

  function identity(value: Record<string, unknown>): string {
    return JSON.stringify(value.name ?? value.listen);
  }

  async function readAgain() {
    try {
      const files = await api<Files>("config/files");
      const found = files.entities.find((other) => other.kind === kind && other.key === key);
      // The key of a server is its position: after a server was added or
      // removed above it, the same key is another host.
      if (kind === "server" && (!found || identity(found.value) !== identity(stored.value))) {
        onreread?.("The file changed and this host is no longer where it was: open the host again.");
        return;
      }
      onreread?.();
      if (!found) {
        failure = { status: 404, message: `The ${noun} is no longer in the configuration.` };
        return;
      }
      stored = {
        value: found.value,
        toml: found.toml,
        revision: files.files.find((other) => other.index === found.file)?.revision ?? "",
      };
      reset();
      failure = undefined;
    } catch (err) {
      fail(err);
    }
  }

  function cancel() {
    if (!dirty || confirm("Discard the unsaved changes?")) oncancel();
  }

  function setCount(keys: string[]): string {
    const set = keys.filter((field) => draft[field] !== undefined).length;
    return set > 0 ? ` (${set} set)` : "";
  }
</script>

{#snippet draw(fieldKey: string)}
  {#if kind === "server" && ACTIONS.includes(fieldKey)}
    <!-- A server does one of the three: they are drawn together, once. -->
    {#if fieldKey === ACTIONS[0]}<Action {schema} {fields} bind:value={draft} />{/if}
  {:else}
    <SchemaField
      {schema}
      node={fields[fieldKey] ?? {}}
      label={fieldKey in fields ? human(fieldKey) : fieldKey}
      name={fieldKey in fields ? fieldKey : undefined}
      help={fields[fieldKey] && helpOf(schema, fieldKey, fields[fieldKey])}
      path={fieldKey}
      bind:value={() => draft[fieldKey], (next) => setKey(fieldKey, next)}
    />
  {/if}
{/snippet}

<section class="editor" aria-labelledby="{id}-title">
  <h2 id="{id}-title">{title}</h2>

  {#if failure}
    <div class="failure" role="alert">
      <p>{failure.message}</p>
      {#if failure.status === 401}
        <p>Enter the admin token, then save again: nothing typed here is lost.</p>
      {/if}
      {#if failure.status === 409 && key !== undefined}
        <p>Reading again replaces what is typed here with what the file holds now.</p>
        <button type="button" onclick={readAgain}>Read again</button>
      {/if}
    </div>
  {/if}

  <div class="mode" role="radiogroup" aria-label="Edit as">
    <label>
      <input
        type="radio"
        name="{id}-mode"
        checked={mode === "form"}
        onclick={(event) => switchTo(event, "form")}
      />
      Form
    </label>
    <label>
      <input
        type="radio"
        name="{id}-mode"
        checked={mode === "toml"}
        onclick={(event) => switchTo(event, "toml")}
      />
      TOML
    </label>
  </div>

  {#if key === undefined && kind !== "server" && named === undefined}
    <div class="name">
      <label for="{id}-name">Name</label>
      <input id="{id}-name" type="text" autocomplete="off" bind:value={name} />
    </div>
  {/if}

  {#if mode === "form"}
    {#key generation}
      {#if groups.common.length > 0}
        <fieldset>
          <legend>{groups.groups.length > 0 ? "Common" : "Settings"}</legend>
          {#each groups.common as fieldKey (fieldKey)}{@render draw(fieldKey)}{/each}
        </fieldset>
      {/if}
      {#each groups.groups as group (group.title)}
        <details class="group">
          <summary>{group.title}{setCount(group.keys)}</summary>
          {#each group.keys as fieldKey (fieldKey)}{@render draw(fieldKey)}{/each}
        </details>
      {/each}
    {/key}
  {:else}
    <div class="toml">
      <label for="{id}-toml">TOML of the table</label>
      <textarea id="{id}-toml" rows="20" spellcheck="false" bind:value={toml}></textarea>
    </div>
  {/if}

  <div class="actions">
    <button type="button" class="primary" disabled={busy} onclick={save}>Save and reload</button>
    <button type="button" disabled={busy} onclick={cancel}>Cancel</button>
    {#if key !== undefined && !(kind === "section" && key === "top")}
      <button type="button" class="danger" disabled={busy} onclick={remove}>Delete</button>
    {/if}
  </div>
</section>

<style>
  h2 {
    margin: 0 0 0.5rem;
    font-size: 1.1rem;
    overflow-wrap: anywhere;
  }

  .failure {
    margin: 0.5rem 0;
    padding: 0.5rem 0.75rem;
    border: 1px solid var(--danger);
    border-radius: 6px;
    color: var(--danger);
  }

  .failure p {
    margin: 0 0 0.25rem;
    /* A reload failure has one line per problem. */
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .mode {
    display: flex;
    gap: 1rem;
    margin: 0.5rem 0;
  }

  .name,
  .toml {
    display: grid;
    gap: 0.2rem;
    margin: 0.5rem 0;
  }

  input[type="text"],
  textarea {
    width: 100%;
    padding: 0.3rem 0.45rem;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--background);
    color: inherit;
    font: inherit;
  }

  .name input {
    max-width: 28rem;
  }

  textarea {
    font-family: ui-monospace, monospace;
  }

  fieldset,
  .group {
    min-width: 0;
    margin: 0.75rem 0;
    padding: 0.25rem 0.75rem 0.5rem;
    border: 1px solid var(--border);
    border-radius: 6px;
  }

  summary {
    padding: 0.25rem 0;
    font-weight: 600;
    cursor: pointer;
  }

  .actions {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
    margin-top: 1rem;
  }

  button {
    padding: 0.35rem 0.8rem;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--surface);
    cursor: pointer;
  }

  .primary {
    border-color: var(--accent);
    color: var(--accent);
  }

  .danger {
    margin-left: auto;
    border-color: var(--danger);
    color: var(--danger);
  }
</style>
