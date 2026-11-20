import { fireEvent, render, screen } from "@testing-library/svelte";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import Hosts from "./Hosts.svelte";
import { serve, writes } from "./serve";

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
});

test("the_editor_offers_the_pools_of_the_file_as_targets", async () => {
  serve({ status: STATUS, "config/files": files([WEB]) });
  render(Hosts);
  await fireEvent.click(await screen.findByRole("button", { name: "Edit web" }));
  expect(screen.getByRole("heading", { name: "Server web" })).toBeTruthy();
  const pool: HTMLSelectElement = screen.getByLabelText("pool");
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
