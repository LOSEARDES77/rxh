<script lang="ts">
  import { untrack } from "svelte";
  import SchemaField from "./SchemaField.svelte";
  import {
    blank,
    control,
    isTable,
    pickChoice,
    resolve,
    textValue,
    type Control,
    type Node,
    type Schema,
  } from "./schema";

  let {
    schema,
    node,
    value = $bindable(),
    label,
    help = undefined,
    path,
    // The value cannot be unset (an item of a list, an entry of a map): a
    // table is drawn without its "Add" and "Remove" buttons.
    required = false,
  }: {
    schema: Schema;
    node: Node;
    value: unknown;
    label: string;
    help?: string;
    path: string;
    required?: boolean;
  } = $props();

  const id = $props.id();
  const resolved = $derived(resolve(schema, node));

  /** A value the control of its key cannot show: a placeholder where a number goes. */
  function foreign(shape: Control, held: unknown): boolean {
    if (held === undefined) return false;
    switch (shape.kind) {
      case "number":
        return typeof held !== "number";
      case "boolean":
        return typeof held !== "boolean";
      case "text":
      case "enum":
        return typeof held === "object";
      case "table":
      case "map":
        return !isTable(held);
      default:
        return false;
    }
  }

  // Decided once: the control must not change under the cursor while typing.
  const asJson = untrack(() => foreign(control(schema, node), value));
  const shape: Control = $derived(asJson ? { kind: "json" } : control(schema, node));

  const unset = $derived(
    resolved.default === undefined || typeof resolved.default === "object"
      ? "Not set"
      : `Not set (default: ${resolved.default === true ? "yes" : resolved.default === false ? "no" : resolved.default})`,
  );
  const placeholder = $derived(
    resolved.default === undefined || typeof resolved.default === "object"
      ? undefined
      : String(resolved.default),
  );

  /** The first paragraph of the description, then the rest: examples are code. */
  const paragraphs = $derived(
    (help ?? "")
      .split("\n\n")
      .filter((text) => text.trim() !== "")
      .map((text) =>
        text.startsWith("```")
          ? { code: true, text: text.replace(/^```\w*\n|\n?```$/g, "") }
          : { code: false, text: text.replace(/\n/g, " ") },
      ),
  );
  const described = $derived(paragraphs.length > 0 ? `${id}-help` : undefined);

  function table(): Record<string, unknown> {
    return isTable(value) ? value : {};
  }

  function setKey(key: string, next: unknown) {
    const copy = { ...table() };
    if (next === undefined) {
      delete copy[key];
    } else {
      copy[key] = next;
    }
    value = copy;
  }

  // One value where one or many are accepted is drawn as a list of one.
  function items(): unknown[] {
    return value === undefined ? [] : Array.isArray(value) ? value : [value];
  }

  function setItem(index: number, next: unknown) {
    const copy = [...items()];
    copy[index] = next;
    value = copy;
  }

  function removeItem(index: number) {
    const copy = items().filter((_, position) => position !== index);
    value = copy.length > 0 ? copy : undefined;
  }

  let entryName = $state("");

  function addEntry(entry: Node) {
    value = { ...table(), [entryName]: blank(schema, entry) ?? "" };
    entryName = "";
  }

  function removeEntry(key: string) {
    const copy = { ...table() };
    delete copy[key];
    value = Object.keys(copy).length > 0 ? copy : undefined;
  }

  function constant(option: Node): boolean {
    const optionShape = control(schema, option);
    return optionShape.kind === "enum" && optionShape.options.length === 1;
  }

  // The alternative in use. Kept apart from the value: a text alternative
  // that is still empty holds no value to tell it from the others.
  let picked = $state(
    untrack(() => {
      if (shape.kind !== "choice") return -1;
      const options = shape.options.map((option) => option.node);
      if (value !== undefined) return pickChoice(schema, options, value);
      return constant(options[0]) ? -1 : 0;
    }),
  );

  function pick(index: number, options: { node: Node }[]) {
    picked = index;
    value = index < 0 ? undefined : blank(schema, options[index].node);
  }

  let json = $state(untrack(() => (value === undefined ? "" : JSON.stringify(value, null, 2))));
  let jsonError = $state("");

  function parseJson(text: string) {
    json = text;
    jsonError = "";
    if (text.trim() === "") {
      value = undefined;
      return;
    }
    try {
      value = JSON.parse(text);
    } catch (err) {
      const reason = err instanceof Error ? err.message : String(err);
      jsonError = `Not valid JSON, the last valid value is kept: ${reason}`;
    }
  }
</script>

{#snippet hint()}
  {#if paragraphs.length > 0}
    <div class="help">
      <p id={described}>{paragraphs[0].text}</p>
      {#if paragraphs.length > 1}
        <details>
          <summary>More about {label}</summary>
          {#each paragraphs.slice(1) as part}
            {#if part.code}<pre>{part.text}</pre>{:else}<p>{part.text}</p>{/if}
          {/each}
        </details>
      {/if}
    </div>
  {/if}
{/snippet}

{#if shape.kind === "text"}
  <div class="field">
    <label for={id}>{label}</label>
    <input
      {id}
      name={path}
      type="text"
      autocomplete="off"
      spellcheck="false"
      value={String(value ?? "")}
      {placeholder}
      aria-describedby={described}
      oninput={(event) => (value = textValue(schema, node, event.currentTarget.value))}
    />
    {@render hint()}
  </div>
{:else if shape.kind === "number"}
  <div class="field">
    <label for={id}>{label}</label>
    <input
      {id}
      name={path}
      type="number"
      step={shape.integer ? 1 : "any"}
      value={String(value ?? "")}
      {placeholder}
      aria-describedby={described}
      oninput={(event) => {
        const text = event.currentTarget.value;
        value = text === "" ? undefined : Number(text);
      }}
    />
    {@render hint()}
  </div>
{:else if shape.kind === "boolean"}
  <div class="field">
    <label for={id}>{label}</label>
    <!-- Not a checkbox: "not set" is a third answer (`http3` unset is not `false`). -->
    <select
      {id}
      name={path}
      value={value === undefined ? "" : value ? "yes" : "no"}
      aria-describedby={described}
      onchange={(event) => {
        const answer = event.currentTarget.value;
        value = answer === "" ? undefined : answer === "yes";
      }}
    >
      <option value="">{unset}</option>
      <option value="yes">Yes</option>
      <option value="no">No</option>
    </select>
    {@render hint()}
  </div>
{:else if shape.kind === "enum"}
  <div class="field">
    <label for={id}>{label}</label>
    <select
      {id}
      name={path}
      value={String(value ?? "")}
      aria-describedby={described}
      onchange={(event) => (value = event.currentTarget.value || undefined)}
    >
      <option value="">{unset}</option>
      {#each shape.options as option (option)}
        <option value={option}>{option}</option>
      {/each}
      {#if typeof value === "string" && !shape.options.includes(value)}
        <option {value}>{value}</option>
      {/if}
    </select>
    {@render hint()}
  </div>
{:else if shape.kind === "list"}
  {@const count = items().length}
  <fieldset>
    <legend>{label}</legend>
    {@render hint()}
    <!-- Keyed by the length too: after a removal the fields are drawn anew from the values. -->
    {#each items() as item, index (`${index}/${count}`)}
      <div class="row">
        <SchemaField
          {schema}
          node={shape.item}
          label="{label} {index + 1}"
          path="{path}.{index}"
          required
          bind:value={() => item, (next) => setItem(index, next)}
        />
        <button
          type="button"
          aria-label="Remove {label} {index + 1}"
          onclick={() => removeItem(index)}
        >
          Remove
        </button>
      </div>
    {/each}
    <button
      type="button"
      onclick={() => (value = [...items(), blank(schema, shape.item)])}
    >
      Add {label}
    </button>
  </fieldset>
{:else if shape.kind === "table"}
  {#if value === undefined && !required}
    <div class="field">
      <button type="button" aria-describedby={described} onclick={() => (value = {})}>
        Add {label}
      </button>
      {@render hint()}
    </div>
  {:else}
    <fieldset>
      <legend>{label}</legend>
      {@render hint()}
      {#each Object.entries(shape.fields) as [key, field] (key)}
        <SchemaField
          {schema}
          node={field}
          label={key}
          help={resolve(schema, field).description}
          path="{path}.{key}"
          bind:value={() => table()[key], (next) => setKey(key, next)}
        />
      {/each}
      <!-- Keys the schema does not know (aliases) are shown and kept. -->
      {#each Object.keys(table()).filter((key) => !(key in shape.fields)) as key (key)}
        <SchemaField
          {schema}
          node={{}}
          label={key}
          path="{path}.{key}"
          bind:value={() => table()[key], (next) => setKey(key, next)}
        />
      {/each}
      {#if !required}
        <button type="button" onclick={() => (value = undefined)}>Remove {label}</button>
      {/if}
    </fieldset>
  {/if}
{:else if shape.kind === "map"}
  <fieldset>
    <legend>{label}</legend>
    {@render hint()}
    {#each Object.keys(table()) as key (key)}
      <div class="row">
        <SchemaField
          {schema}
          node={shape.value}
          label={key}
          path="{path}.{key}"
          required
          bind:value={
            () => table()[key],
            (next) => (value = { ...table(), [key]: next ?? "" })
          }
        />
        <button type="button" aria-label="Remove {label} {key}" onclick={() => removeEntry(key)}>
          Remove
        </button>
      </div>
    {/each}
    <div class="row">
      <div class="field">
        <label for="{id}-new">New {label} name</label>
        <input id="{id}-new" type="text" autocomplete="off" bind:value={entryName} />
      </div>
      <button
        type="button"
        disabled={entryName === "" || Object.hasOwn(table(), entryName)}
        onclick={() => addEntry(shape.value)}
      >
        Add {label}
      </button>
    </div>
  </fieldset>
{:else if shape.kind === "choice"}
  <fieldset>
    <legend>{label}</legend>
    {@render hint()}
    <div class="field">
      <label for="{id}-as">Written as</label>
      <select
        id="{id}-as"
        value={String(picked)}
        onchange={(event) => pick(Number(event.currentTarget.value), shape.options)}
      >
        <option value="-1">Not set</option>
        {#each shape.options as option, index (index)}
          <option value={String(index)}>{option.label}</option>
        {/each}
      </select>
    </div>
    {#if picked >= 0 && !constant(shape.options[picked].node)}
      {#key picked}
        <SchemaField {schema} node={shape.options[picked].node} {label} {path} required bind:value />
      {/key}
    {/if}
  </fieldset>
{:else}
  <div class="field">
    <label for={id}>{label} (JSON)</label>
    <textarea
      {id}
      name={path}
      rows="4"
      spellcheck="false"
      value={json}
      aria-invalid={jsonError !== ""}
      aria-describedby={described}
      oninput={(event) => parseJson(event.currentTarget.value)}
    ></textarea>
    {#if jsonError}<p class="error" role="alert">{jsonError}</p>{/if}
    {@render hint()}
  </div>
{/if}

<style>
  .field {
    display: grid;
    grid-template-columns: minmax(8rem, 14rem) minmax(0, 28rem);
    gap: 0.2rem 0.75rem;
    align-items: start;
    margin: 0.5rem 0;
  }

  .field > :not(label) {
    grid-column: 2;
  }

  label,
  legend {
    font-family: ui-monospace, monospace;
    overflow-wrap: anywhere;
  }

  input,
  select,
  textarea {
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

  fieldset {
    min-width: 0;
    margin: 0.75rem 0;
    padding: 0.25rem 0.75rem 0.5rem;
    border: 1px solid var(--border);
    border-radius: 6px;
  }

  .row {
    display: flex;
    gap: 0.5rem;
    align-items: start;
  }

  .row > :global(:first-child) {
    flex: 1;
    min-width: 0;
  }

  .row > button {
    margin-top: 0.5rem;
  }

  button {
    justify-self: start;
    padding: 0.3rem 0.7rem;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--surface);
    cursor: pointer;
  }

  .help {
    color: var(--muted);
    font-size: 0.85rem;
  }

  .help p {
    margin: 0;
  }

  .help pre {
    margin: 0.25rem 0;
    overflow-x: auto;
  }

  .error {
    margin: 0;
    color: var(--danger);
  }

  @media (max-width: 40rem) {
    .field {
      grid-template-columns: minmax(0, 1fr);
    }

    .field > :not(label) {
      grid-column: 1;
    }
  }
</style>
