<script lang="ts">
  import SchemaField from "../SchemaField.svelte";
  import { alternatives, control, isTable, type Node, type Schema } from "../schema";

  // A key written as one text, or as a table whose key `main` holds that text.
  let {
    schema,
    node,
    value = $bindable(),
    label,
    path,
    main,
    text,
  }: {
    schema: Schema;
    node: Node;
    value: unknown;
    label: string;
    path: string;
    main: string;
    text: string;
  } = $props();

  const table = $derived(
    alternatives(schema, node).find((option) => control(schema, option).kind === "table") ?? {},
  );
</script>

{#if isTable(value)}
  {@const held = value}
  <SchemaField {schema} node={table} label="{label} options" {path} required bind:value />
  <button type="button" class="button" onclick={() => (value = held[main])}>
    Only the {text} (drops the other {label} options)
  </button>
{:else if value === undefined || typeof value === "string"}
  {@const held = value}
  <SchemaField {schema} node={{ type: "string" }} label={text} {path} bind:value />
  <button
    type="button"
    class="button"
    onclick={() => (value = held === undefined ? {} : { [main]: held })}
  >
    More {label} options
  </button>
{:else}
  <SchemaField {schema} node={{}} label={text} {path} bind:value />
{/if}
