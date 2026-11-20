<script lang="ts">
  import { getContext, untrack } from "svelte";
  import SchemaField from "../SchemaField.svelte";
  import { alternatives, control, isTable, pickChoice, type Node, type Schema } from "../schema";

  let {
    schema,
    node,
    value = $bindable(),
    label,
    path,
  }: { schema: Schema; node: Node; value: unknown; label: string; path: string } = $props();

  type Mode = "none" | "pool" | "address" | "several" | "advanced";
  const MODES: { key: Mode; text: string }[] = [
    { key: "none", text: "None" },
    { key: "pool", text: "Pool" },
    { key: "address", text: "Address" },
    { key: "several", text: "Several addresses" },
    { key: "advanced", text: "Advanced" },
  ];

  const id = $props.id();
  // The pools of the configuration, given by the tab that draws the editor.
  const pools = getContext<(() => string[]) | undefined>("pools");
  // What is not one text: a list of backends, a weighted split, the table of an inline pool.
  const others = untrack(() =>
    alternatives(schema, node).filter((option) => control(schema, option).kind !== "text"),
  );
  const table = others.find((option) => control(schema, option).kind === "table") ?? {};

  function modeOf(held: unknown): Mode {
    if (held === undefined) return "none";
    if (typeof held === "string") return held.startsWith("@") ? "pool" : "address";
    const texts = Array.isArray(held) && held.every((item) => typeof item === "string");
    return texts ? "several" : "advanced";
  }

  // Kept apart from the value: an address not typed yet holds no value.
  let mode = $state(untrack(() => modeOf(value)));
  // Decided once: a row being typed must not turn a split into a list of backends.
  let advanced = $state.raw(
    untrack(() => (Array.isArray(value) ? others[pickChoice(schema, others, value)] : table)),
  );

  const names = $derived((pools?.() ?? []).map((name) => `@${name}`));

  function pick(next: Mode) {
    mode = next;
    advanced = table;
    value = next === "advanced" ? {} : undefined;
  }
</script>

<div class="choices" role="radiogroup" aria-label={label}>
  {#each MODES as choice (choice.key)}
    <label>
      <input
        type="radio"
        name="{id}-mode"
        checked={mode === choice.key}
        onchange={() => pick(choice.key)}
      />
      {choice.text}
    </label>
  {/each}
</div>
{#key mode}
  {#if mode === "pool"}
    <SchemaField {schema} node={{ enum: names }} label="pool" {path} bind:value />
  {:else if mode === "address"}
    <SchemaField {schema} node={{ type: "string" }} label="address" {path} bind:value />
  {:else if mode === "several"}
    <SchemaField
      {schema}
      node={{ type: "array", items: { type: "string" } }}
      label="addresses"
      {path}
      bind:value
    />
  {:else if mode === "advanced"}
    <SchemaField {schema} node={advanced} label="{label} settings" {path} required bind:value />
  {/if}
{/key}
