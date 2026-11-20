/** A node of the configuration JSON schema (`GET schema`), as schemars writes it. */
export type Node = {
  $ref?: string;
  type?: string | string[];
  description?: string;
  default?: unknown;
  const?: unknown;
  enum?: unknown[];
  properties?: Record<string, Node | boolean>;
  required?: string[];
  additionalProperties?: Node | boolean;
  items?: Node;
  anyOf?: Node[];
  oneOf?: Node[];
  allOf?: Node[];
};

export type Schema = Node & { definitions?: Record<string, Node> };

export type Kind = "server" | "upstream" | "section";

/** One table of the configuration, as `GET config/files` lists it. */
export type Entity = {
  kind: Kind;
  key: string;
  file: number;
  value: Record<string, unknown>;
  toml: string;
};

/** The answer of `GET config/files`. */
export type Files = {
  editable: boolean;
  files: { index: number; path: string; revision: string }[];
  entities: Entity[];
};

export type Control =
  | { kind: "text" }
  | { kind: "number"; integer: boolean }
  | { kind: "boolean" }
  | { kind: "enum"; options: string[] }
  | { kind: "list"; item: Node } // arrays, and "one or many" shown as a list
  | { kind: "table"; fields: Record<string, Node>; required: string[] }
  | { kind: "map"; value: Node } // additionalProperties
  | { kind: "choice"; options: { label: string; node: Node }[] } // anyOf / oneOf
  | { kind: "json" }; // fallback

const SCALARS = ["string", "integer", "number"];

export function isTable(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function typesOf(node: Node): string[] {
  return [node.type ?? []].flat().filter((type) => type !== "null");
}

/** Follows `$ref` and merges `allOf`; the keys of the node itself win. */
export function resolve(schema: Schema, node: Node): Node {
  const { $ref, allOf, ...own } = node;
  let merged: Node = own;
  if ($ref || allOf) {
    const name = $ref?.replace("#/definitions/", "");
    const parts = [...(name ? [schema.definitions?.[name] ?? {}] : []), ...(allOf ?? [])].map(
      (part) => resolve(schema, part),
    );
    merged = Object.assign({}, ...parts, own);
    const tables = [...parts, own].filter((part) => part.properties);
    if (tables.length > 1) {
      merged.properties = Object.assign({}, ...tables.map((part) => part.properties));
      merged.required = tables.flatMap((part) => part.required ?? []);
    }
  }
  if (merged.properties && merged.oneOf) {
    // A table whose `oneOf` says which action key it holds (`Pattern`): the
    // alternatives carry the schema of the keys the table only lists as `true`.
    const properties = { ...merged.properties };
    for (const alternative of merged.oneOf) {
      for (const [key, field] of Object.entries(resolve(schema, alternative).properties ?? {})) {
        if (properties[key] === true) properties[key] = field;
      }
    }
    const { oneOf: _, ...rest } = merged;
    merged = { ...rest, properties };
  }
  return merged;
}

function same(schema: Schema, left: Node, right: Node): boolean {
  return JSON.stringify(resolve(schema, left)) === JSON.stringify(resolve(schema, right));
}

function words(name: string): string {
  const spaced = name.replace(/([a-z0-9])([A-Z])/g, "$1 $2").toLowerCase();
  return spaced.charAt(0).toUpperCase() + spaced.slice(1);
}

function labelOf(schema: Schema, node: Node): string {
  if (node.$ref) return words(node.$ref.replace("#/definitions/", ""));
  const constant = resolve(schema, node).const;
  if (constant !== undefined) return String(constant);
  const shape = control(schema, node);
  if (shape.kind === "list") return `List of ${labelOf(schema, shape.item).toLowerCase()}`;
  const names = {
    text: "Text",
    number: "Number",
    boolean: "Yes or no",
    enum: "Choice",
    table: "Table",
    map: "Map",
    choice: "Choice",
    json: "JSON",
  };
  return names[shape.kind];
}

/** The form control that edits the values of `node`. */
export function control(schema: Schema, node: Node): Control {
  const resolved = resolve(schema, node);
  const variants = resolved.anyOf ?? resolved.oneOf;
  if (variants) {
    // `null` among the alternatives only makes the key optional.
    const options = variants.filter((variant) => resolve(schema, variant).type !== "null");
    if (options.length === 1) return control(schema, options[0]);
    const shapes = options.map((option) => resolve(schema, option));
    if (shapes.every((shape) => typeof shape.const === "string")) {
      return { kind: "enum", options: shapes.map((shape) => shape.const as string) };
    }
    if (shapes.every((shape) => typeof shape.type === "string" && SCALARS.includes(shape.type))) {
      return { kind: "text" };
    }
    if (options.length === 2) {
      const many = shapes.findIndex((shape) => shape.type === "array" && shape.items);
      const one = options[1 - many];
      if (many >= 0 && same(schema, shapes[many].items ?? {}, one)) {
        return { kind: "list", item: one };
      }
    }
    return {
      kind: "choice",
      options: options.map((option) => ({ label: labelOf(schema, option), node: option })),
    };
  }
  if (typeof resolved.const === "string") return { kind: "enum", options: [resolved.const] };
  if (resolved.enum?.every((option) => typeof option === "string")) {
    return { kind: "enum", options: resolved.enum as string[] };
  }
  const types = typesOf(resolved);
  if (types.length > 1) {
    // A duration or a size: a number or a string with a unit.
    return types.every((type) => SCALARS.includes(type)) ? { kind: "text" } : { kind: "json" };
  }
  switch (types[0]) {
    case "string":
      return { kind: "text" };
    case "integer":
    case "number":
      return { kind: "number", integer: types[0] === "integer" };
    case "boolean":
      return { kind: "boolean" };
    case "array":
      return resolved.items ? { kind: "list", item: resolved.items } : { kind: "json" };
    case "object":
      if (resolved.properties) {
        const fields: Record<string, Node> = {};
        for (const [key, field] of Object.entries(resolved.properties)) {
          fields[key] = typeof field === "object" ? field : {};
        }
        return { kind: "table", fields, required: resolved.required ?? [] };
      }
      if (typeof resolved.additionalProperties === "object") {
        return { kind: "map", value: resolved.additionalProperties };
      }
  }
  return { kind: "json" };
}

function fits(schema: Schema, node: Node, value: unknown): boolean {
  const shape = control(schema, node);
  switch (shape.kind) {
    case "text":
      return typeof value === "string" || typeof value === "number";
    case "number":
      return typeof value === "number";
    case "boolean":
      return typeof value === "boolean";
    case "enum":
      return typeof value === "string" && shape.options.includes(value);
    case "list":
      return Array.isArray(value) && value.every((item) => fits(schema, shape.item, item));
    case "table":
      return (
        isTable(value) &&
        Object.keys(value).every((key) => key in shape.fields) &&
        shape.required.every((key) => key in value)
      );
    case "map":
      return isTable(value);
    case "choice":
      return shape.options.some((option) => fits(schema, option.node, value));
    case "json":
      return true;
  }
}

/** Control kinds that can hold a value of each JSON type, for values no alternative fits. */
const LOOSE: Record<string, string[]> = {
  string: ["text", "enum"],
  number: ["number", "text"],
  boolean: ["boolean"],
  array: ["list"],
  object: ["table", "map"],
};

/** Index of the alternative that `value` is written as; 0 when none matches. */
export function pickChoice(schema: Schema, options: Node[], value: unknown): number {
  const exact = options.findIndex((option) => fits(schema, option, value));
  if (exact >= 0) return exact;
  // A placeholder (`{env.X}`) or an alias fits no alternative exactly.
  const kinds = LOOSE[Array.isArray(value) ? "array" : typeof value] ?? [];
  const loose = options.findIndex((option) => kinds.includes(control(schema, option).kind));
  return Math.max(loose, 0);
}

/** The schema of one entity of `GET config/files`. */
export function entitySchema(schema: Schema, kind: Kind, key: string): Node {
  const root = resolve(schema, schema).properties ?? {};
  const property = (name: string): Node => {
    const node = root[name];
    return typeof node === "object" ? node : {};
  };
  if (kind === "server") return resolve(schema, property("server")).items ?? {};
  if (kind === "upstream") {
    const pool = resolve(schema, property("upstream")).additionalProperties;
    return typeof pool === "object" ? pool : {};
  }
  if (key !== "top") return property(key);
  // `top` holds the root keys that are not tables of their own.
  const properties: Record<string, Node> = {};
  for (const name of Object.keys(root)) {
    const shape = control(schema, property(name));
    const tables = shape.kind === "list" && control(schema, shape.item).kind === "table";
    if (shape.kind !== "table" && shape.kind !== "map" && !tables) {
      properties[name] = property(name);
    }
  }
  return { type: "object", properties };
}

/** What a field holds when it is first drawn: a table, a constant, or nothing. */
export function blank(schema: Schema, node: Node): unknown {
  const shape = control(schema, node);
  if (shape.kind === "table") return {};
  if (shape.kind === "enum" && shape.options.length === 1) return shape.options[0];
  return undefined;
}

/**
 * The value of a text box: nothing when empty, and a number when the schema
 * takes one there (`5` seconds, status `404`).
 */
export function textValue(schema: Schema, node: Node, text: string): unknown {
  if (text === "") return undefined;
  const resolved = resolve(schema, node);
  const types = [resolved, ...(resolved.anyOf ?? []).map((option) => resolve(schema, option))]
    .flatMap(typesOf);
  return types.includes("integer") && /^[0-9]+$/.test(text) ? Number(text) : text;
}

/** Drops unset keys and list items: the API refuses `null`. */
export function prune(value: unknown): unknown {
  if (Array.isArray(value)) {
    const kept = value.map(prune).filter((item) => item !== undefined && item !== null);
    // Rows added to a list and left blank do not make it `[]`: the key stays unset.
    return value.length > 0 && kept.length === 0 ? undefined : kept;
  }
  if (isTable(value)) {
    const kept: Record<string, unknown> = {};
    for (const [key, raw] of Object.entries(value)) {
      const item = prune(raw);
      if (item !== undefined && item !== null) kept[key] = item;
    }
    return kept;
  }
  return value;
}

/** The name of the definition a key refers to, through `null` alternatives and `allOf`. */
export function definition(node: Node): string | undefined {
  const parts = [node, ...(node.anyOf ?? []), ...(node.allOf ?? [])];
  return parts.find((part) => part.$ref)?.$ref?.replace("#/definitions/", "");
}

/** The alternatives of a key written in several shapes, without `null`. */
export function alternatives(schema: Schema, node: Node): Node[] {
  const shape = control(schema, node);
  return shape.kind === "choice" ? shape.options.map((option) => option.node) : [];
}
