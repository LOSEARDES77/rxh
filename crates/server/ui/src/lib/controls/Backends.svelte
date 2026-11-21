<script lang="ts">
  import SchemaField from "../SchemaField.svelte";
  import { isTable, type Schema } from "../schema";

  // The backends of a pool: one row per backend, an address and a weight.
  let {
    schema,
    value = $bindable(),
    path,
  }: { schema: Schema; node: unknown; value: unknown; label: string; path: string } = $props();

  const id = $props.id();
  const items: unknown[] = $derived(
    value === undefined ? [] : Array.isArray(value) ? value : [value],
  );

  function setItem(index: number, next: unknown) {
    const copy = [...items];
    copy[index] = next;
    value = copy;
  }

  function remove(index: number) {
    const copy = items.filter((_, position) => position !== index);
    value = copy.length > 0 ? copy : undefined;
  }

  /** `item` as a table with `key` set, or without it when `next` is nothing. */
  function withKey(item: unknown, key: string, next: unknown): Record<string, unknown> {
    const table: Record<string, unknown> =
      isTable(item) ? { ...item } : item === undefined ? {} : { address: item };
    if (next === undefined) {
      delete table[key];
    } else {
      table[key] = next;
    }
    return table;
  }

  // A backend written as one text stays one text until it gets a weight.
  function setAddress(index: number, text: string) {
    const item = items[index];
    setItem(index, isTable(item) ? withKey(item, "address", text || undefined) : text || undefined);
  }

  function setWeight(index: number, text: string) {
    const item = items[index];
    if (text === "" && !isTable(item)) return;
    // Not a number box: a placeholder (`{env.WEIGHT}`) is shown and kept.
    const weight = text === "" ? undefined : /^[0-9]+$/.test(text) ? Number(text) : text;
    setItem(index, withKey(item, "weight", weight));
  }

  function extras(item: unknown): string[] {
    return isTable(item) ? Object.keys(item).filter((key) => key !== "address" && key !== "weight") : [];
  }
</script>

<!-- Keyed by the length too: after a removal the rows are drawn anew from the values. -->
{#each items as item, index (`${index}/${items.length}`)}
  {@const table = isTable(item) ? item : undefined}
  <div class="backend">
    <label for="{id}-address-{index}">Backend {index + 1}</label>
    <input
      id="{id}-address-{index}"
      name="{path}.{index}"
      type="text"
      autocomplete="off"
      spellcheck="false"
      placeholder="host:port"
      value={String((table ? table.address : item) ?? "")}
      oninput={(event) => setAddress(index, event.currentTarget.value)}
    />
    <label for="{id}-weight-{index}">Weight</label>
    <input
      id="{id}-weight-{index}"
      aria-label="Weight of backend {index + 1}"
      class="weight"
      type="text"
      inputmode="numeric"
      autocomplete="off"
      placeholder="1"
      value={String(table?.weight ?? "")}
      oninput={(event) => setWeight(index, event.currentTarget.value)}
    />
    <button
      type="button"
      class="button"
      aria-label="Remove backend {index + 1}"
      onclick={() => remove(index)}
    >
      Remove
    </button>
  </div>
  {#if table && extras(table).length > 0}
    <details>
      <summary>More options of backend {index + 1}</summary>
      {#each extras(table) as key (key)}
        <SchemaField
          {schema}
          node={{}}
          label={key}
          path="{path}.{index}.{key}"
          bind:value={() => table[key], (next) => setItem(index, withKey(table, key, next))}
        />
      {/each}
    </details>
  {/if}
{/each}
<button type="button" class="button" onclick={() => (value = [...items, undefined])}>
  Add backend
</button>

<style>
  .backend {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.4rem 0.6rem;
    margin: 0.4rem 0;
  }

  .backend label:first-child {
    width: 6rem;
  }

  input {
    flex: 1 1 14rem;
    min-width: 0;
    max-width: 24rem;
    padding: 0.3rem 0.45rem;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--background);
  }

  .weight {
    flex: 0 0 5rem;
  }

  label + input + label {
    color: var(--muted);
  }

  details {
    margin: 0 0 0.5rem 6.6rem;
  }

  summary {
    color: var(--muted);
    cursor: pointer;
  }
</style>
