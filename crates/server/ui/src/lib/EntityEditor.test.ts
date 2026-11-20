import { fireEvent, render, screen } from "@testing-library/svelte";
import { get } from "svelte/store";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import rxhSchema from "../../../../../docs/rxh.schema.json";
import { needsToken, setToken } from "./api";
import EntityEditor from "./EntityEditor.svelte";
import { control, entitySchema, type Entity, type Kind, type Schema } from "./schema";

const schema = rxhSchema as unknown as Schema;

const API: Entity = {
  kind: "upstream",
  key: "api",
  file: 0,
  // `lb_retries` stands for an alias: a key of the file the schema does not know.
  value: { backends: ["127.0.0.1:8080"], policy: "round_robin", secret: "***", lb_retries: 2 },
  toml: 'backends = ["127.0.0.1:8080"]\npolicy = "round_robin"\n',
};

/** Answers every write with `status` and `body`, and `GET config/files` with `files`. */
function serve(status: number, body: unknown, files?: unknown) {
  const fetch = vi.fn(async (url: string, _init: RequestInit) =>
    url === "config/files"
      ? Response.json(files)
      : new Response(JSON.stringify(body), { status }),
  );
  vi.stubGlobal("fetch", fetch);
  return fetch;
}

function sent(fetch: ReturnType<typeof serve>, call = 0) {
  const [url, init] = fetch.mock.calls[call];
  return {
    url,
    method: init.method,
    body: typeof init.body === "string" ? JSON.parse(init.body) : undefined,
  };
}

function open(entity: Entity | { kind: Kind; file: number } = API, revision = "r1") {
  const onsaved = vi.fn();
  const oncancel = vi.fn();
  const { container } = render(EntityEditor, { schema, entity, revision, onsaved, oncancel });
  return { container, onsaved, oncancel };
}

function type(element: HTMLElement, text: string) {
  return fireEvent.input(element, { target: { value: text } });
}

function save() {
  return fireEvent.click(screen.getByRole("button", { name: "Save and reload" }));
}

function field(label: string): HTMLInputElement {
  return screen.getByLabelText(label);
}

let confirm: ReturnType<typeof vi.fn<(message?: string) => boolean>>;

beforeEach(() => {
  sessionStorage.clear();
  confirm = vi.fn(() => true);
  vi.stubGlobal("confirm", confirm);
});

afterEach(() => {
  vi.unstubAllGlobals();
  setToken("");
});

test("save_sends_the_whole_table_and_the_revision", async () => {
  const fetch = serve(200, { saved: true });
  const { onsaved } = open();
  await fireEvent.change(field("policy"), { target: { value: "ip_hash" } });
  await type(field("backends 1"), "127.0.0.1:9090");
  await save();
  await vi.waitFor(() => expect(onsaved).toHaveBeenCalledTimes(1));
  expect(fetch).toHaveBeenCalledTimes(1);
  expect(sent(fetch)).toEqual({
    url: "config/entities/upstream/api",
    method: "PUT",
    // The masked secret and the key no field draws go back as they came.
    body: {
      value: { backends: ["127.0.0.1:9090"], policy: "ip_hash", secret: "***", lb_retries: 2 },
      revision: "r1",
    },
  });
});

test("the_key_is_percent_encoded", async () => {
  const fetch = serve(200, { saved: true });
  const first = open({ ...API, key: "a/b c" });
  await save();
  await vi.waitFor(() => expect(first.onsaved).toHaveBeenCalled());
  expect(sent(fetch).url).toBe("config/entities/upstream/a%2Fb%20c");

  const second = open({ ...API, key: "a.b" });
  await fireEvent.click(screen.getAllByRole("button", { name: "Save and reload" })[1]);
  await vi.waitFor(() => expect(second.onsaved).toHaveBeenCalled());
  expect(sent(fetch, 1).url).toBe("config/entities/upstream/a.b");
});

test("a_400_shows_the_message_and_keeps_the_typed_value", async () => {
  serve(400, { error: "upstream api: no backends\nthe file was left as it was" });
  const { onsaved } = open();
  await type(field("key"), "x-user");
  await save();
  const alert = await screen.findByRole("alert");
  expect(alert.textContent).toContain("upstream api: no backends\nthe file was left as it was");
  expect(field("key").value).toBe("x-user");
  expect(onsaved).not.toHaveBeenCalled();
  expect(screen.queryByRole("button", { name: "Read again" })).toBeNull();
});

test("a_409_offers_to_read_again_and_the_next_save_uses_the_new_revision", async () => {
  const files = {
    editable: true,
    files: [{ index: 0, path: "rxh.toml", revision: "r2", text: "" }],
    entities: [{ ...API, value: { backends: ["10.0.0.1:80"] }, toml: 'backends = ["10.0.0.1:80"]\n' }],
  };
  const fetch = serve(409, { error: "the file changed since it was read" }, files);
  open();
  await type(field("backends 1"), "typed:1");
  await save();
  expect((await screen.findByRole("alert")).textContent).toContain("the file changed");
  expect(field("backends 1").value).toBe("typed:1");

  await fireEvent.click(screen.getByRole("button", { name: "Read again" }));
  await vi.waitFor(() => expect(field("backends 1").value).toBe("10.0.0.1:80"));
  expect(screen.queryByRole("alert")).toBeNull();
  await save();
  await vi.waitFor(() => expect(fetch).toHaveBeenCalledTimes(3));
  expect(sent(fetch, 2).body).toEqual({ value: { backends: ["10.0.0.1:80"] }, revision: "r2" });
});

test("a_401_keeps_the_editor_and_the_typed_value_behind_the_token_dialog", async () => {
  serve(401, { error: "missing or wrong token" });
  const { onsaved, oncancel } = open();
  await type(field("key"), "x-user");
  await save();
  await screen.findByRole("alert");
  expect(get(needsToken)).toBe(true);
  expect(field("key").value).toBe("x-user");
  expect(onsaved).not.toHaveBeenCalled();
  expect(oncancel).not.toHaveBeenCalled();
});

test("the_toml_view_sends_the_text", async () => {
  const fetch = serve(200, { saved: true });
  const { onsaved } = open();
  await fireEvent.click(screen.getByRole("radio", { name: "TOML" }));
  // Nothing was typed: nothing to discard, nothing to ask.
  expect(confirm).not.toHaveBeenCalled();
  const text: HTMLTextAreaElement = screen.getByLabelText("TOML of the table");
  expect(text.value).toBe(API.toml);
  await type(text, 'backends = ["127.0.0.1:1"]\n');
  await save();
  await vi.waitFor(() => expect(onsaved).toHaveBeenCalled());
  expect(sent(fetch).body).toEqual({ toml: 'backends = ["127.0.0.1:1"]\n', revision: "r1" });
});

test("switching_views_with_unsaved_edits_asks_before_discarding_them", async () => {
  open();
  expect(screen.getByRole("radiogroup")).toBeTruthy();
  await type(field("key"), "x-user");

  confirm.mockReturnValue(false);
  await fireEvent.click(screen.getByRole("radio", { name: "TOML" }));
  expect(confirm).toHaveBeenCalledTimes(1);
  expect(field("key").value).toBe("x-user");
  expect(screen.queryByLabelText("TOML of the table")).toBeNull();

  confirm.mockReturnValue(true);
  await fireEvent.click(screen.getByRole("radio", { name: "TOML" }));
  expect(screen.getByLabelText("TOML of the table")).toBeTruthy();
  await fireEvent.click(screen.getByRole("radio", { name: "Form" }));
  // The edit was discarded, not converted.
  expect(field("key").value).toBe("");
});

test("delete_asks_for_confirmation", async () => {
  const fetch = serve(200, { saved: true });
  const { onsaved } = open();
  confirm.mockReturnValue(false);
  await fireEvent.click(screen.getByRole("button", { name: "Delete" }));
  expect(confirm).toHaveBeenCalledTimes(1);
  expect(fetch).not.toHaveBeenCalled();

  confirm.mockReturnValue(true);
  await fireEvent.click(screen.getByRole("button", { name: "Delete" }));
  await vi.waitFor(() => expect(onsaved).toHaveBeenCalled());
  expect(sent(fetch)).toEqual({
    url: "config/entities/upstream/api?revision=r1",
    method: "DELETE",
    body: undefined,
  });
});

test("cancel_asks_only_when_something_was_typed", async () => {
  const { oncancel } = open();
  await fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
  expect(confirm).not.toHaveBeenCalled();
  expect(oncancel).toHaveBeenCalledTimes(1);

  await type(field("key"), "x-user");
  confirm.mockReturnValue(false);
  await fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
  expect(oncancel).toHaveBeenCalledTimes(1);
});

test("a_new_pool_is_posted_with_its_name_and_file", async () => {
  const fetch = serve(201, { key: "web" });
  const { onsaved } = open({ kind: "upstream", file: 1 });
  expect(screen.queryByRole("button", { name: "Delete" })).toBeNull();
  await type(field("Name"), "web");
  await fireEvent.click(screen.getByRole("button", { name: "Add backends" }));
  await type(field("backends 1"), "127.0.0.1:8080");
  await save();
  await vi.waitFor(() => expect(onsaved).toHaveBeenCalled());
  expect(sent(fetch)).toEqual({
    url: "config/entities/upstream",
    method: "POST",
    body: { value: { backends: ["127.0.0.1:8080"] }, key: "web", file: 1 },
  });
});

test("a_new_server_has_no_name_to_give", async () => {
  const fetch = serve(201, { key: "0:3" });
  const { onsaved } = open({ kind: "server", file: 0 });
  expect(screen.queryByLabelText("Name")).toBeNull();
  await save();
  await vi.waitFor(() => expect(onsaved).toHaveBeenCalled());
  expect(sent(fetch).body).toEqual({ value: {}, file: 0 });
});

test("every_key_is_reachable_and_every_control_has_a_label", () => {
  for (const [kind, key] of [
    ["server", "0:0"],
    ["upstream", "api"],
    ["section", "acme"],
    ["section", "top"],
  ] as const) {
    const value = kind === "server" ? { listen: ":443", tls: "auto", old_alias: true } : {};
    const { container } = open({ kind, key, file: 0, value, toml: "" });
    const names = [...container.querySelectorAll("label, legend, button")].map((element) =>
      element.textContent?.trim().replace(/^Add /, ""),
    );
    const shape = control(schema, entitySchema(schema, kind, key));
    const keys = shape.kind === "table" ? Object.keys(shape.fields) : [];
    expect(keys.length).toBeGreaterThan(3);
    expect(keys.filter((name) => !names.includes(name))).toEqual([]);
    if (kind === "server") expect(names).toContain("old_alias (JSON)");

    const controls = container.querySelectorAll<HTMLInputElement>("input, select, textarea");
    expect(controls.length).toBeGreaterThan(3);
    expect([...controls].filter((element) => element.labels?.length !== 1)).toEqual([]);
  }
});

test("the_groups_of_a_server_follow_the_layout", () => {
  const { container } = open({ kind: "server", key: "0:0", file: 0, value: { odd: 1 }, toml: "" });
  const titles = [...container.querySelectorAll("details.group > summary")].map((summary) => summary.textContent);
  expect(titles.map((title) => title?.trim())).toEqual([
    "Access",
    "Routes",
    "Headers and rewrites",
    "TLS and protocols",
    "Performance",
    "Timeouts and limits",
    "Everything else (1 set)",
  ]);
});
