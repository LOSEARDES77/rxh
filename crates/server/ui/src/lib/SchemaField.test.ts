import { fireEvent, render, screen } from "@testing-library/svelte";
import { SvelteMap } from "svelte/reactivity";
import { expect, test } from "vitest";
import rxhSchema from "../../../../../docs/rxh.schema.json";
import SchemaField from "./SchemaField.svelte";
import type { Node, Schema } from "./schema";

const schema = rxhSchema as unknown as Schema;
const server = schema.definitions?.Server.properties as Record<string, Node>;
const pool = schema.definitions?.UpstreamGroup.properties as Record<string, Node>;

/** Draws one field bound to a value the test can read, as a parent component would. */
function draw(node: Node, initial: unknown, label = "field", help?: string) {
  const box = new SvelteMap<string, unknown>([["value", initial]]);
  const view = render(SchemaField, {
    props: {
      schema,
      node,
      label,
      help,
      path: label,
      get value() {
        return box.get("value");
      },
      set value(next: unknown) {
        box.set("value", next);
      },
    },
  });
  return { container: view.container, value: () => box.get("value") };
}

function type(element: HTMLElement, text: string) {
  return fireEvent.input(element, { target: { value: text } });
}

test("typing_in_a_text_field_updates_the_value", async () => {
  const field = draw({ type: "string" }, undefined, "name");
  await type(screen.getByLabelText("name"), "example.org");
  expect(field.value()).toBe("example.org");
});

test("clearing_an_optional_field_removes_the_key", async () => {
  const node: Node = {
    type: "object",
    properties: { name: { type: ["string", "null"] }, port: { type: "integer" } },
  };
  const field = draw(node, { name: "web", port: 8080 }, "table");
  expect(screen.getByRole("group", { name: "table" })).toBeTruthy();
  await type(screen.getByLabelText("name"), "");
  expect(field.value()).toEqual({ port: 8080 });
  expect(Object.keys(field.value() as object)).toEqual(["port"]);
  await type(screen.getByLabelText("port"), "");
  expect(field.value()).toEqual({});
  await type(screen.getByLabelText("port"), "9090");
  expect(field.value()).toEqual({ port: 9090 });
});

test("adding_and_removing_list_items", async () => {
  const field = draw(server.host, ["a.example"], "host");
  expect(screen.getByRole("group", { name: "host" })).toBeTruthy();
  await fireEvent.click(screen.getByRole("button", { name: "Add host" }));
  await type(screen.getByLabelText("host 2"), "b.example");
  expect(field.value()).toEqual(["a.example", "b.example"]);
  await fireEvent.click(screen.getByRole("button", { name: "Remove host 1" }));
  expect(field.value()).toEqual(["b.example"]);
  expect((screen.getByLabelText("host 1") as HTMLInputElement).value).toBe("b.example");
  expect(screen.queryByLabelText("host 2")).toBeNull();
  await fireEvent.click(screen.getByRole("button", { name: "Remove host 1" }));
  expect(field.value()).toBeUndefined();
});

test("one_value_is_drawn_as_a_list_and_stays_a_list_once_edited", async () => {
  const field = draw(server.listen, ":443", "listen");
  await type(screen.getByLabelText("listen 1"), ":8443");
  expect(field.value()).toEqual([":8443"]);
});

test("text_of_the_configuration_is_never_markup", () => {
  const attack = "<img src=x onerror=alert(1)>";
  const node: Node = {
    type: "object",
    properties: {
      name: { type: "string" },
      policy: { enum: ["round_robin"] },
      users: { type: "object", additionalProperties: { type: "string" } },
      hosts: { type: "array", items: { type: "string" } },
      other: {},
    },
  };
  const value = { name: attack, policy: attack, users: { [attack]: attack }, hosts: [attack], other: attack };
  const { container } = draw(node, value, attack, attack);
  expect(container.querySelector("img")).toBeNull();
  expect((screen.getByLabelText("name") as HTMLInputElement).value).toBe(attack);
  expect(container.textContent).toContain(attack);
});

test("a_duration_is_a_number_when_only_digits_are_typed", async () => {
  const field = draw(pool.dial_timeout, "3s", "dial_timeout");
  const input = screen.getByLabelText("dial_timeout") as HTMLInputElement;
  expect(input.value).toBe("3s");
  await type(input, "5");
  expect(field.value()).toBe(5);
  await type(input, "250ms");
  expect(field.value()).toBe("250ms");
});

test("a_number_field_holds_a_number", async () => {
  const field = draw(pool.retries, undefined, "retries");
  await type(screen.getByLabelText("retries"), "3");
  expect(field.value()).toBe(3);
  await type(screen.getByLabelText("retries"), "");
  expect(field.value()).toBeUndefined();
});

test("a_boolean_is_yes_no_or_not_set", async () => {
  const field = draw(server.http3, undefined, "http3");
  const select = screen.getByLabelText("http3") as HTMLSelectElement;
  expect(select.value).toBe("");
  await fireEvent.change(select, { target: { value: "no" } });
  expect(field.value()).toBe(false);
  await fireEvent.change(select, { target: { value: "yes" } });
  expect(field.value()).toBe(true);
  await fireEvent.change(select, { target: { value: "" } });
  expect(field.value()).toBeUndefined();
});

test("an_enum_keeps_a_value_the_schema_does_not_list", async () => {
  const field = draw(pool.policy, "{env.POLICY}", "policy");
  const select = screen.getByLabelText("policy") as HTMLSelectElement;
  expect(select.value).toBe("{env.POLICY}");
  await fireEvent.change(select, { target: { value: "ip_hash" } });
  expect(field.value()).toBe("ip_hash");
});

test("a_choice_switches_between_its_alternatives", async () => {
  // Under another path than `tls` the key has no control of its own.
  const field = draw(server.tls, undefined, "field");
  const written = screen.getByLabelText("Written as") as HTMLSelectElement;
  expect(written.value).toBe("-1");
  await fireEvent.change(written, { target: { value: "0" } });
  expect(field.value()).toBe("auto");
  await fireEvent.change(written, { target: { value: "1" } });
  expect(field.value()).toEqual({});
  await type(screen.getByLabelText("cert"), "/etc/cert.pem");
  expect(field.value()).toEqual({ cert: "/etc/cert.pem" });
  await fireEvent.change(written, { target: { value: "-1" } });
  expect(field.value()).toBeUndefined();
});

test("a_choice_opens_on_the_alternative_of_its_value", () => {
  draw(server.forward, [{ pool: "v1", weight: 90 }], "field");
  expect((screen.getByLabelText("Written as") as HTMLSelectElement).value).toBe("2");
  expect((screen.getByLabelText("pool") as HTMLInputElement).value).toBe("v1");
});

test("an_optional_table_is_added_and_removed", async () => {
  const field = draw(pool.health, undefined, "health");
  expect(screen.queryByRole("group", { name: "health" })).toBeNull();
  await fireEvent.click(screen.getByRole("button", { name: "Add health" }));
  // An empty table is a value: `[upstream.api.health]` turns the checks on.
  expect(field.value()).toEqual({});
  await type(screen.getByLabelText("interval"), "10s");
  expect(field.value()).toEqual({ interval: "10s" });
  await fireEvent.click(screen.getByRole("button", { name: "Remove health" }));
  expect(field.value()).toBeUndefined();
});

test("a_table_shows_and_keeps_the_keys_the_schema_does_not_know", async () => {
  const field = draw(pool.health, { uri: "/up", health_uri: "/old" }, "health");
  expect((screen.getByLabelText("health_uri (JSON)") as HTMLTextAreaElement).value).toBe('"/old"');
  await type(screen.getByLabelText("uri"), "/ready");
  expect(field.value()).toEqual({ uri: "/ready", health_uri: "/old" });
});

test("map_entries_are_added_and_removed", async () => {
  const node: Node = { type: "object", additionalProperties: { type: "string" } };
  const field = draw(node, { "x-a": "1" }, "header");
  await type(screen.getByLabelText("x-a"), "2");
  expect(field.value()).toEqual({ "x-a": "2" });
  await type(screen.getByLabelText("New header name"), "x-b");
  await fireEvent.click(screen.getByRole("button", { name: "Add header" }));
  await type(screen.getByLabelText("x-b"), "3");
  expect(field.value()).toEqual({ "x-a": "2", "x-b": "3" });
  await fireEvent.click(screen.getByRole("button", { name: "Remove header x-a" }));
  expect(field.value()).toEqual({ "x-b": "3" });
});

test("the_json_fallback_reports_a_parse_error_and_keeps_the_last_valid_value", async () => {
  const field = draw({}, { a: 1 }, "odd");
  const box = screen.getByLabelText("odd (JSON)") as HTMLTextAreaElement;
  expect(JSON.parse(box.value)).toEqual({ a: 1 });
  await type(box, '{"a": ');
  expect(screen.getByRole("alert").textContent).toContain("Not valid JSON");
  expect(field.value()).toEqual({ a: 1 });
  await type(box, '{"a": 2}');
  expect(screen.queryByRole("alert")).toBeNull();
  expect(field.value()).toEqual({ a: 2 });
  await type(box, "");
  expect(field.value()).toBeUndefined();
});

test("a_value_of_another_type_than_the_schema_is_edited_as_json", () => {
  draw(pool.retries, "{env.RETRIES}", "retries");
  expect((screen.getByLabelText("retries (JSON)") as HTMLTextAreaElement).value).toBe(
    '"{env.RETRIES}"',
  );
});

test("help_is_tied_to_its_input", () => {
  draw({ type: "string" }, undefined, "name", "Shown in logs.\n\n```toml\nname = \"web\"\n```");
  const input = screen.getByLabelText("name");
  const help = document.getElementById(input.getAttribute("aria-describedby") ?? "");
  expect(help?.textContent).toContain("Shown in logs.");
});

test("removing_a_list_item_redraws_the_ones_after_it_from_their_values", async () => {
  const field = draw(pool.backends, ["127.0.0.1:1", { address: "127.0.0.1:2", weight: 3 }], "backends");
  await fireEvent.click(screen.getByRole("button", { name: "Remove backends 1" }));
  expect(field.value()).toEqual([{ address: "127.0.0.1:2", weight: 3 }]);
  // The first item is now the table, not the text it was.
  expect((screen.getByLabelText("Written as") as HTMLSelectElement).value).toBe("1");
  expect((screen.getByLabelText("address") as HTMLInputElement).value).toBe("127.0.0.1:2");
});

test("a_value_that_is_not_the_constant_of_its_alternative_is_edited_as_json", () => {
  const field = draw(server.tls, "{env.TLS}", "field");
  expect(screen.queryByLabelText("Written as")).toBeNull();
  expect((screen.getByLabelText("field (JSON)") as HTMLTextAreaElement).value).toBe('"{env.TLS}"');
  expect(field.value()).toBe("{env.TLS}");
});

test("a_text_with_newlines_is_edited_in_a_textarea", async () => {
  const field = draw({ type: "string" }, "line one\nline two", "body");
  const box = screen.getByLabelText("body");
  expect(box.tagName).toBe("TEXTAREA");
  expect((box as HTMLTextAreaElement).value).toBe("line one\nline two");
  await type(box, "line one\nline 2");
  expect(field.value()).toBe("line one\nline 2");
});

test("the_keys_with_a_control_of_their_own_are_not_drawn_as_a_choice", () => {
  draw(server.tls, "auto", "tls");
  expect(screen.queryByLabelText("Written as")).toBeNull();
  expect((screen.getByRole("radio", { name: "Automatic" }) as HTMLInputElement).checked).toBe(true);
});

test("the_tls_table_of_a_pool_keeps_the_generic_control", () => {
  draw(pool.tls, { server_name: "db.internal" }, "tls");
  expect(screen.queryByRole("radio")).toBeNull();
  expect((screen.getByLabelText("server_name") as HTMLInputElement).value).toBe("db.internal");
});
