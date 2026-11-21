import { fireEvent, render, screen } from "@testing-library/svelte";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { session } from "../lib/session";
import Hosts from "./Hosts.svelte";
import { serve, who, writes } from "./serve";

const STATUS = {
  servers: [
    { listen: "127.0.0.1:8080", protocol: "http", name: "web", state: "listening" },
    { listen: "127.0.0.1:9000", protocol: "tcp", name: "docker-db", state: "listening" },
  ],
};

const WEB = {
  kind: "server",
  key: "0:0",
  file: 0,
  value: { name: "web", listen: "127.0.0.1:8080", host: ["a.example", "b.example"], forward: "@api", tls: "auto" },
  toml: "",
};
const FILES_SERVER = {
  kind: "server",
  key: "0:1",
  file: 0,
  value: { listen: ["127.0.0.1:8443", "127.0.0.1:8444"], serve: { root: "/var/www" } },
  toml: "",
};

function files(entities: unknown[], revision = "r1") {
  return {
    editable: true,
    files: [{ index: 0, path: "/etc/rxh/rxh.toml", revision, text: "" }],
    entities: [...entities, { kind: "upstream", key: "api", file: 0, value: {}, toml: "" }],
  };
}

let confirm: ReturnType<typeof vi.fn<(message?: string) => boolean>>;

beforeEach(() => {
  session.set(undefined);
  confirm = vi.fn(() => true);
  vi.stubGlobal("confirm", confirm);
});

afterEach(() => vi.unstubAllGlobals());

function row(text: string): string {
  return screen.getByText(text).closest("tr")?.textContent?.replace(/\s+/g, " ") ?? "";
}

test("a_row_per_server_of_the_file_with_its_live_state", async () => {
  serve({ status: STATUS, "config/files": files([WEB, FILES_SERVER]) });
  render(Hosts);
  await screen.findByRole("button", { name: "Edit web" });
  const web = row("a.example, b.example");
  for (const text of ["web", "127.0.0.1:8080", "http", "@api", "automatic", "listening", "rxh.toml"]) {
    expect(web).toContain(text);
  }
  const served = row("files of /var/www");
  for (const text of ["127.0.0.1:8443, 127.0.0.1:8444", "any", "off", "not running"]) {
    expect(served).toContain(text);
  }
});

test("a_runtime_server_is_listed_read_only", async () => {
  serve({ status: STATUS, "config/files": files([WEB]) });
  render(Hosts);
  await screen.findByRole("button", { name: "Edit web" });
  const runtime = row("docker-db");
  expect(runtime).toContain("runtime");
  expect(runtime).toContain("127.0.0.1:9000");
  expect(runtime).toContain("tcp");
  expect(screen.getAllByRole("button", { name: /^(Edit|Delete) / })).toHaveLength(2);
});

test("without_a_configuration_file_every_edit_button_is_hidden", async () => {
  serve({ status: STATUS, "config/files": { editable: false, files: [], entities: [] } });
  render(Hosts);
  expect(await screen.findByText("No configuration file: read only")).toBeTruthy();
  expect(row("docker-db")).toContain("runtime");
  expect(row("web")).toContain("runtime");
  expect(screen.queryByRole("button")).toBeNull();
});

test("delete_asks_then_sends_the_revision_of_the_list", async () => {
  const fetch = serve({ status: STATUS, "config/files": files([WEB]) });
  render(Hosts);
  confirm.mockReturnValue(false);
  await fireEvent.click(await screen.findByRole("button", { name: "Delete web" }));
  expect(confirm.mock.calls[0][0]).toContain("web");
  expect(writes(fetch)).toEqual([]);

  confirm.mockReturnValue(true);
  await fireEvent.click(screen.getByRole("button", { name: "Delete web" }));
  await vi.waitFor(() => expect(fetch.mock.calls.filter(([url]) => url === "config/files").length).toBe(2));
  expect(writes(fetch)).toEqual([{ url: "config/entities/server/0%3A0?revision=r1", method: "DELETE", body: undefined }]);
});

test("a_refused_delete_is_reported_and_the_list_is_read_again", async () => {
  const fetch = serve({ status: STATUS, "config/files": files([WEB]) }, { status: 409, body: { error: "the file changed since it was read" } });
  render(Hosts);
  await fireEvent.click(await screen.findByRole("button", { name: "Delete web" }));
  expect((await screen.findByRole("alert")).textContent).toContain("the file changed");
  expect(fetch.mock.calls.filter(([url]) => url === "config/files").length).toBe(2);
  // The reads that follow work: they must not take the refusal away.
  const reads = () => fetch.mock.calls.filter(([url]) => url === "status").length;
  const before = reads();
  document.dispatchEvent(new Event("visibilitychange"));
  await vi.waitFor(() => expect(reads()).toBeGreaterThan(before));
  await new Promise((done) => setTimeout(done, 20));
  expect(screen.getByRole("alert").textContent).toContain("the file changed");
});

test("the_editor_offers_the_pools_of_the_file_as_targets", async () => {
  serve({ status: STATUS, "config/files": files([WEB]) });
  render(Hosts);
  await fireEvent.click(await screen.findByRole("button", { name: "Edit web" }));
  expect(screen.getByRole("heading", { name: "Server web" })).toBeTruthy();
  const pool: HTMLSelectElement = screen.getByLabelText("Pool", { selector: "select" });
  expect([...pool.options].map((option) => option.value)).toContain("@api");
  await fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
  expect(screen.getByRole("button", { name: "New host" })).toBeTruthy();
});

test("after_a_conflict_reading_again_keeps_the_editor_only_on_the_same_host", async () => {
  const answers = { status: STATUS, "config/files": files([WEB, FILES_SERVER]) };
  const fetch = serve(answers, { status: 409, body: { error: "the file changed since it was read" } });
  render(Hosts);
  await fireEvent.click(await screen.findByRole("button", { name: "Edit web" }));
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload" }));

  // The same host is still first: the editor stays, on the new revision.
  answers["config/files"] = files([{ ...WEB, value: { ...WEB.value, tls: undefined } }, FILES_SERVER], "r2");
  await fireEvent.click(await screen.findByRole("button", { name: "Read again" }));
  await vi.waitFor(() => expect((screen.getByRole("radio", { name: "Off" }) as HTMLInputElement).checked).toBe(true));
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload" }));
  await vi.waitFor(() => expect(writes(fetch)).toHaveLength(2));
  expect(writes(fetch)[1].body.revision).toBe("r2");

  // A host was removed above it: `0:0` is now another host.
  answers["config/files"] = files([FILES_SERVER], "r3");
  await fireEvent.click(await screen.findByRole("button", { name: "Read again" }));
  expect((await screen.findByRole("status")).textContent).toContain("open the host again");
  expect(screen.queryByRole("heading", { name: "Server web" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Delete" })).toBeNull();
  expect(await screen.findByRole("button", { name: "Edit 127.0.0.1:8443, 127.0.0.1:8444" })).toBeTruthy();
});

function live(listen: string, name: string | null = null, state = "listening") {
  return { listen, protocol: "http", name, state };
}

test("a_server_with_two_listen_addresses_owns_both_listeners", async () => {
  const two = { ...FILES_SERVER, value: { listen: ["127.0.0.1:8443", "127.0.0.1:8444"], serve: "." } };
  serve({
    status: { servers: [live("127.0.0.1:8443"), live("127.0.0.1:8444", null, "starting")] },
    "config/files": files([two]),
  });
  render(Hosts);
  await screen.findByRole("button", { name: "Edit 127.0.0.1:8443, 127.0.0.1:8444" });
  await vi.waitFor(() => expect(row("127.0.0.1:8443, 127.0.0.1:8444")).toContain("listening, starting"));
  expect(screen.queryByText("runtime")).toBeNull();
  expect(screen.getAllByRole("row")).toHaveLength(2);
});

test("a_named_server_owns_the_listeners_that_carry_its_name", async () => {
  // Port 0 and a placeholder: the addresses of the file match nothing.
  const named = { ...WEB, value: { name: "web", listen: ["127.0.0.1:0", "{env.LISTEN}"], forward: "@api" } };
  serve({
    status: { servers: [live("127.0.0.1:40001", "web"), live("127.0.0.1:40002", "web, other"), live("127.0.0.1:9", "docker")] },
    "config/files": files([named]),
  });
  render(Hosts);
  await screen.findByRole("button", { name: "Edit web" });
  await vi.waitFor(() => expect(row("127.0.0.1:0, {env.LISTEN}")).toContain("listening"));
  expect(screen.getAllByRole("row")).toHaveLength(3);
  expect(row("docker")).toContain("runtime");
});

test("an_unnamed_server_on_port_0_is_unknown_and_its_listener_is_not_called_runtime", async () => {
  const vague = { ...FILES_SERVER, value: { listen: "127.0.0.1:0", serve: "." } };
  serve({
    status: { servers: [live("127.0.0.1:40001"), live("127.0.0.1:9", "docker")] },
    "config/files": files([vague]),
  });
  render(Hosts);
  await screen.findByRole("button", { name: "Edit 127.0.0.1:0" });
  await vi.waitFor(() => expect(row("127.0.0.1:40001")).toContain("not matched"));
  expect(row("127.0.0.1:0")).toContain("unknown");
  expect(row("127.0.0.1:40001")).not.toContain("runtime");
  // A listener with a name no server of the files has is a runtime one for sure.
  expect(row("docker")).toContain("runtime");
});

/** `GET config/files` as a user without the `admin` role gets it. */
function reduced(entities: unknown[]) {
  return {
    editable: true,
    files: [{ index: 0, revision: "r1" }],
    entities: [...entities, { kind: "upstream", key: "api" }],
  };
}

function host(position: number, name: string, permissions: string[], groups = ["web"]) {
  return {
    kind: "server",
    key: `0:${position}`,
    file: 0,
    value: { name, listen: "127.0.0.1:8080", host: `${name}.example`, forward: "@api", groups },
    toml: "",
    permissions,
  };
}

test("the_list_shows_the_groups_of_each_host", async () => {
  serve({ status: STATUS, "config/files": files([host(0, "shop", ["view", "modify", "create", "delete"], ["web", "paid"]), FILES_SERVER]) });
  render(Hosts);
  await screen.findByRole("button", { name: "Edit shop" });
  expect(screen.getByRole("columnheader", { name: "Groups" })).toBeTruthy();
  expect(row("shop.example")).toContain("web, paid");
});

test("buttons_follow_the_permissions_of_each_host", async () => {
  session.set(who({ user: "luis", roles: ["web-editors"] }));
  const entities = [host(0, "seen", ["view"]), host(1, "changed", ["view", "modify"]), host(2, "removed", ["view", "delete"])];
  serve({ status: { servers: [] }, "config/files": reduced(entities) });
  render(Hosts);
  await screen.findByRole("button", { name: "View seen" });
  const names = screen.getAllByRole("button").map((button) => button.getAttribute("aria-label"));
  expect(names).toEqual(["View seen", "Edit changed", "View removed", "Delete removed"]);
  // No path of a file is told to this user, and none is made up.
  expect(document.body.textContent).not.toContain("undefined");

  await fireEvent.click(screen.getByRole("button", { name: "View seen" }));
  expect(screen.getByRole("heading", { name: "Server seen" })).toBeTruthy();
  expect(screen.queryByRole("button", { name: "Save and reload" })).toBeNull();
  expect(screen.queryByRole("button", { name: "Delete" })).toBeNull();
  expect(screen.queryByRole("radio", { name: "TOML" })).toBeNull();
  expect((screen.getByLabelText("Name") as HTMLInputElement).matches(":disabled")).toBe(true);
  // The pools are names only, and still offered.
  const pool: HTMLSelectElement = screen.getByLabelText("Pool", { selector: "select" });
  expect([...pool.options].map((option) => option.value)).toContain("@api");
  await fireEvent.click(screen.getByRole("button", { name: "Close" }));

  await fireEvent.click(screen.getByRole("button", { name: "Edit changed" }));
  expect(screen.getByRole("button", { name: "Save and reload" })).toBeTruthy();
  expect(screen.getByRole("radio", { name: "TOML" })).toBeTruthy();
  expect(screen.queryByRole("button", { name: "Delete" })).toBeNull();
  expect((screen.getByLabelText("Name") as HTMLInputElement).matches(":disabled")).toBe(false);
  await fireEvent.click(screen.getByRole("button", { name: "Cancel" }));

  await fireEvent.click(screen.getByRole("button", { name: "View removed" }));
  expect(screen.queryByRole("button", { name: "Save and reload" })).toBeNull();
  expect(screen.getByRole("button", { name: "Delete" })).toBeTruthy();
});

test("a_save_refused_for_a_key_shows_its_path_and_keeps_the_form", async () => {
  session.set(who({ user: "luis", roles: ["web-editors"] }));
  serve(
    { status: { servers: [] }, "config/files": reduced([host(0, "changed", ["view", "modify"])]) },
    { status: 403, body: { error: "this needs the admin role: serve" } },
  );
  render(Hosts);
  await fireEvent.click(await screen.findByRole("button", { name: "Edit changed" }));
  await fireEvent.click(screen.getByRole("radio", { name: "TOML" }));
  await fireEvent.input(screen.getByLabelText("TOML of the table"), { target: { value: 'serve = "/etc"' } });
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload" }));
  expect((await screen.findByRole("alert")).textContent).toContain("this needs the admin role: serve");
  expect((screen.getByLabelText("TOML of the table") as HTMLTextAreaElement).value).toBe('serve = "/etc"');
});

test("new_host_starts_in_a_creatable_group", async () => {
  session.set(who({ user: "luis", roles: ["web-editors"], creatable_groups: ["web", "shop"] }));
  const fetch = serve({ status: { servers: [] }, "config/files": reduced([]) });
  render(Hosts);
  await fireEvent.click(await screen.findByRole("button", { name: "New host" }));
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload" }));
  await vi.waitFor(() => expect(writes(fetch)).toHaveLength(1));
  expect(writes(fetch)[0]).toEqual({ url: "config/entities/server", method: "POST", body: { value: { groups: ["web"] }, file: 0 } });
});

test("no_new_host_without_a_group_to_create_in", async () => {
  session.set(who({ user: "luis", roles: ["viewers"] }));
  serve({ status: { servers: [] }, "config/files": reduced([host(0, "seen", ["view"])]) });
  render(Hosts);
  await screen.findByRole("button", { name: "View seen" });
  expect(screen.queryByRole("button", { name: "New host" })).toBeNull();
});
