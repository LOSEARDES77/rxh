<script lang="ts">
  import SchemaField from "../SchemaField.svelte";
  import { alternatives, control, isTable, type Node, type Schema } from "../schema";

  let {
    schema,
    node,
    value = $bindable(),
    label,
    path,
  }: { schema: Schema; node: Node; value: unknown; label: string; path: string } = $props();

  const id = $props.id();
  const files = $derived(
    alternatives(schema, node).find((option) => control(schema, option).kind === "table") ?? {},
  );
  const mode = $derived(
    value === undefined ? "off" : value === "auto" ? "auto" : isTable(value) ? "files" : "written",
  );
  // A value that is none of the three (a placeholder) keeps a radio of its own.
  const modes = $derived([
    { key: "off", text: "Off", write: undefined },
    { key: "auto", text: "Automatic", write: "auto" },
    { key: "files", text: "Certificate files", write: {} },
    ...(mode === "written" ? [{ key: "written", text: "As written", write: value }] : []),
  ]);
</script>

<div class="choices" role="radiogroup" aria-label={label}>
  {#each modes as choice (choice.key)}
    <label>
      <input
        type="radio"
        name="{id}-mode"
        checked={mode === choice.key}
        onchange={() => (value = choice.write)}
      />
      {choice.text}
    </label>
  {/each}
</div>
{#if mode === "files"}
  <SchemaField {schema} node={files} label="Certificate files" {path} required bind:value />
{:else if mode === "written"}
  <SchemaField
    {schema}
    node={typeof value === "string" ? { type: "string" } : {}}
    label="{label} as written"
    {path}
    bind:value
  />
{/if}
