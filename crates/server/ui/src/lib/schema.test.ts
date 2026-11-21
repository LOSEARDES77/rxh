import { expect, test } from "vitest";
import document from "../../../../../docs/rxh.schema.json";
import { arrange, layout } from "./layout";
import {
  blank,
  control,
  entitySchema,
  pickChoice,
  prune,
  resolve,
  textValue,
  type Node,
  type Schema,
} from "./schema";

const schema = document as unknown as Schema;

function definition(name: string): Node {
  return { $ref: `#/definitions/${name}` };
}

function property(name: string, key: string): Node {
  const shape = control(schema, definition(name));
  if (shape.kind !== "table") throw new Error(`${name} is not a table`);
  return shape.fields[key];
}

/** Properties that are edited as JSON. Keep it empty: every shape has a control. */
const JSON_FALLBACK: string[] = [];

test("every_property_of_every_definition_has_a_control_that_is_not_json", () => {
  const fallback: string[] = [];
  const seen = new Set<string>();
  let properties = 0;
  // Items of lists, values of maps and alternatives of choices count too.
  const visit = (node: Node, path: string) => {
    const shape = control(schema, node);
    if (shape.kind === "json") fallback.push(path);
    if (shape.kind === "list") visit(shape.item, `${path}[]`);
    if (shape.kind === "map") visit(shape.value, `${path}{}`);
    if (shape.kind === "choice") {
      for (const option of shape.options) visit(option.node, `${path}|${option.label}`);
    }
    if (shape.kind === "table") {
      // Each table once: a match entry holds match entries.
      const table = JSON.stringify(shape.fields);
      if (seen.has(table)) return;
      seen.add(table);
      for (const [key, field] of Object.entries(shape.fields)) {
        properties += 1;
        visit(field, `${path}.${key}`);
      }
    }
  };
  visit(schema, "root");
  for (const name of Object.keys(schema.definitions ?? {})) visit(definition(name), name);
  expect(properties).toBeGreaterThan(280);
  expect(fallback).toEqual(JSON_FALLBACK);
});

test("duration_and_size_are_text", () => {
  expect(control(schema, definition("Duration"))).toEqual({ kind: "text" });
  expect(control(schema, definition("Size"))).toEqual({ kind: "text" });
  expect(control(schema, property("UpstreamGroup", "dial_timeout"))).toEqual({ kind: "text" });
  // Optional: `anyOf` with `null`.
  expect(control(schema, property("UpstreamGroup", "flush_interval"))).toEqual({ kind: "text" });
  expect(control(schema, property("RequestConfig", "max_body_size"))).toEqual({ kind: "text" });
});

test("policy_is_an_enum", () => {
  const policy = control(schema, property("UpstreamGroup", "policy"));
  expect(policy.kind).toBe("enum");
  expect(policy.kind === "enum" && policy.options).toContain("round_robin");
  expect(policy.kind === "enum" && policy.options).toContain("ip_hash");
});

test("host_is_a_list", () => {
  const host = control(schema, property("Server", "host"));
  expect(host.kind).toBe("list");
  expect(host.kind === "list" && control(schema, host.item)).toEqual({ kind: "text" });
  expect(control(schema, property("Server", "listen")).kind).toBe("list");
  // One table or many.
  const mirror = control(schema, property("Server", "mirror"));
  expect(mirror.kind === "list" && control(schema, mirror.item).kind).toBe("table");
});

test("scalars_optional_values_and_maps", () => {
  expect(control(schema, property("UpstreamGroup", "retries"))).toEqual({
    kind: "number",
    integer: true,
  });
  expect(control(schema, property("Tracing", "sample_ratio"))).toEqual({
    kind: "number",
    integer: false,
  });
  expect(control(schema, property("Server", "http3"))).toEqual({ kind: "boolean" });
  expect(control(schema, property("Acme", "email"))).toEqual({ kind: "text" });
  expect(control(schema, property("UpstreamGroup", "health")).kind).toBe("table");
  expect(control(schema, property("BasicAuth", "users")).kind).toBe("map");
  // A status code or a class such as `5xx`.
  expect(control(schema, definition("StatusMatcher"))).toEqual({ kind: "text" });
  expect(control(schema, {})).toEqual({ kind: "json" });
  expect(control(schema, { type: "array" })).toEqual({ kind: "json" });
});

test("tls_is_a_choice_of_auto_or_files", () => {
  const tls = control(schema, property("Server", "tls"));
  expect(tls.kind === "choice" && tls.options.map((option) => option.label)).toEqual([
    "auto",
    "Tls files",
  ]);
});

test("the_action_of_a_match_entry_takes_its_schema_from_the_alternatives", () => {
  expect(control(schema, property("Pattern", "forward")).kind).toBe("choice");
  expect(control(schema, property("Pattern", "subroute")).kind).toBe("list");
});

test("resolve_follows_references_and_keeps_the_keys_of_the_node", () => {
  const timeout = resolve(schema, property("UpstreamGroup", "dial_timeout"));
  expect(timeout.type).toEqual(["string", "integer"]);
  expect(timeout.description).toBe("Upstream connect timeout.");
  expect(timeout.default).toBe("3s");
  expect(timeout.allOf).toBeUndefined();
});

test("pick_choice_matches_the_value_shape", () => {
  const forward = control(schema, property("Server", "forward"));
  if (forward.kind !== "choice") throw new Error("forward is a choice");
  expect(forward.options.map((option) => option.label)).toEqual([
    "Text",
    "List of backend",
    "List of weighted pool",
    "Upstream group",
  ]);
  const options = forward.options.map((option) => option.node);
  expect(pickChoice(schema, options, "@api")).toBe(0);
  expect(pickChoice(schema, options, ["127.0.0.1:8080", "127.0.0.1:8081"])).toBe(1);
  expect(pickChoice(schema, options, [{ address: "127.0.0.1:8080", weight: 3 }])).toBe(1);
  expect(pickChoice(schema, options, [{ pool: "v1", weight: 90 }])).toBe(2);
  expect(pickChoice(schema, options, { policy: "ip_hash", backends: ["127.0.0.1:8080"] })).toBe(3);
  // An alias the schema does not know still reads as a table.
  expect(pickChoice(schema, options, { lb_policy: "ip_hash" })).toBe(3);
  expect(pickChoice(schema, options, undefined)).toBe(0);

  const fallback = control(schema, property("UpstreamGroup", "fallback"));
  if (fallback.kind !== "choice") throw new Error("fallback is a choice");
  const policies = fallback.options.map((option) => option.node);
  expect(pickChoice(schema, policies, "random")).toBe(0);
  expect(pickChoice(schema, policies, "{env.POLICY}")).toBe(0);
  expect(pickChoice(schema, policies, { policy: "header", key: "x-user" })).toBe(1);
});

test("entity_schema_of_each_kind", () => {
  const fields = (kind: "server" | "upstream" | "section", key: string): string[] => {
    const shape = control(schema, entitySchema(schema, kind, key));
    return shape.kind === "table" ? Object.keys(shape.fields).sort() : [];
  };
  expect(fields("server", "0:0")).toContain("listen");
  expect(fields("upstream", "api")).toContain("backends");
  expect(fields("section", "admin")).toContain("listen");
  // Optional sections are tables too.
  expect(fields("section", "tracing")).toContain("sample_ratio");
  expect(fields("section", "top")).toEqual(["grace_period", "include", "runtime", "watch"]);
});

test("text_value_is_a_number_only_where_the_schema_takes_one", () => {
  expect(textValue(schema, definition("Duration"), "5")).toBe(5);
  expect(textValue(schema, definition("Duration"), "5s")).toBe("5s");
  expect(textValue(schema, property("UpstreamGroup", "flush_interval"), "30")).toBe(30);
  expect(textValue(schema, definition("StatusMatcher"), "404")).toBe(404);
  expect(textValue(schema, definition("StatusMatcher"), "5xx")).toBe("5xx");
  expect(textValue(schema, { type: "string" }, "8080")).toBe("8080");
  expect(textValue(schema, { type: "string" }, "")).toBeUndefined();
});

test("blank_and_prune", () => {
  expect(blank(schema, definition("TlsFiles"))).toEqual({});
  expect(blank(schema, { const: "auto" })).toBe("auto");
  expect(blank(schema, { type: "string" })).toBeUndefined();
  expect(prune({ a: undefined, b: [1, undefined, { c: null, d: "x" }], e: "***" })).toEqual({
    b: [1, { d: "x" }],
    e: "***",
  });
  // Rows added to a list and left blank do not make it `[]`; a stored `[]` stays.
  expect(prune({ hosts: [undefined, undefined], kept: [], nested: { list: [undefined] } })).toEqual({
    kept: [],
    nested: {},
  });
});

test("every_key_of_a_server_and_of_a_pool_is_in_exactly_one_group", () => {
  for (const kind of ["server", "upstream"] as const) {
    const shape = control(schema, entitySchema(schema, kind, ""));
    const keys = shape.kind === "table" ? Object.keys(shape.fields) : [];
    const arranged = arrange(kind, [...keys, "an_alias"]);
    const drawn = [...arranged.common, ...arranged.groups.flatMap((group) => group.keys)];
    expect(drawn.sort()).toEqual([...keys, "an_alias"].sort());
    expect(arranged.common).toEqual(layout[kind].common);
    expect(arranged.groups.at(-1)).toEqual({ title: "Everything else", keys: ["an_alias"] });
  }
  expect(arrange("section", ["listen", "enabled"])).toEqual({
    common: ["listen", "enabled"],
    groups: [],
  });
});
