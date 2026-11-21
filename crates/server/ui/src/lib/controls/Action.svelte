<script lang="ts" module>
  /** The keys of a server that say what it does: the configuration takes one of them. */
  export const ACTIONS = ["forward", "serve", "redirect"];
</script>

<script lang="ts">
  import { untrack } from "svelte";
  import { helpOf, human } from "../layout";
  import SchemaField from "../SchemaField.svelte";
  import type { Node, Schema } from "../schema";

  let {
    schema,
    fields,
    // The whole table of the server.
    value = $bindable(),
  }: { schema: Schema; fields: Record<string, Node>; value: Record<string, unknown> } = $props();

  const id = $props.id();
  const CHOICES = [
    { key: "forward", text: "Forward to" },
    { key: "serve", text: "Serve files" },
    { key: "redirect", text: "Redirect" },
    { key: "none", text: "None" },
  ];

  const present = untrack(() => ACTIONS.filter((key) => value[key] !== undefined));
  // A file that holds several is not valid, but nothing of it is dropped
  // here: each one is drawn, for the person to remove the ones not meant.
  const several = present.length > 1;
  // Kept apart from the value: an action just picked holds no value yet.
  let chosen = $state(present[0] ?? "none");
  let shown = $state(present);

  function pick(next: string) {
    chosen = next;
    const copy = { ...value };
    for (const key of ACTIONS) {
      if (key !== next) delete copy[key];
    }
    value = copy;
  }

  function setKey(key: string, next: unknown) {
    const copy = { ...value };
    if (next === undefined) {
      delete copy[key];
    } else {
      copy[key] = next;
    }
    value = copy;
  }
</script>

{#snippet field(key: string, required: boolean)}
  <SchemaField
    {schema}
    node={fields[key] ?? {}}
    label={human(key)}
    name={key}
    help={fields[key] && helpOf(schema, key, fields[key])}
    path={key}
    {required}
    bind:value={() => value[key], (next) => setKey(key, next)}
  />
{/snippet}

{#if several}
  {#each shown as key (key)}{@render field(key, false)}{/each}
  <div class="choices">
    {#each ACTIONS.filter((key) => !shown.includes(key)) as key (key)}
      <button type="button" class="button" onclick={() => (shown = [...shown, key])}>
        Add {key}
      </button>
    {/each}
  </div>
{:else}
  <div class="choices" role="radiogroup" aria-labelledby="{id}-title">
    <strong id="{id}-title">Action</strong>
    {#each CHOICES as choice (choice.key)}
      <label>
        <input
          type="radio"
          name="{id}-action"
          checked={chosen === choice.key}
          onchange={() => pick(choice.key)}
        />
        {choice.text}
      </label>
    {/each}
  </div>
  {#if chosen === "none"}
    <p class="muted">No action of its own: the routes of the server, under Routes, say what it does.</p>
  {:else}
    {#key chosen}{@render field(chosen, true)}{/key}
  {/if}
{/if}

<style>
  p {
    margin: 0 0 0.5rem;
    font-size: 0.85rem;
  }
</style>
