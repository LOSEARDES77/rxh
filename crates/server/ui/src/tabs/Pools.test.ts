import { fireEvent, render, screen, within } from "@testing-library/svelte";
import { afterEach, expect, test, vi } from "vitest";
import { setToken } from "../lib/api";
import Pools from "./Pools.svelte";
import { serve, writes } from "./serve";

function backend(address: string, more: object = {}) {
  return {
    address,
    weight: 1,
    healthy: true,
    available: true,
    num_requests: 2,
    total_requests: 40,
    fails: 0,
    state: "ready",
    ...more,
  };
}

const POOLS = [
  { name: "api", policy: "round_robin", queued: 0, upstreams: [backend("127.0.0.1:1"), backend("127.0.0.1:2", { healthy: false, fails: 3, state: "drain" }), backend("127.0.0.1:3", { available: false, state: "maintenance" }), backend("127.0.0.1:4", { available: false })] },
  { name: "docker-web", policy: "least_conn", queued: 0, upstreams: [backend("10.0.0.5:80")] },
];

const FILES = {
  editable: true,
  files: [
    { index: 0, path: "/etc/rxh/rxh.toml", revision: "r1", text: "" },
    { index: 1, path: "/etc/rxh/conf.d/extra.toml", revision: "r2", text: "" },
  ],
  entities: [
    { kind: "upstream", key: "api", file: 0, value: { backends: ["127.0.0.1:1", "127.0.0.1:2"] }, toml: "" },
    { kind: "upstream", key: "idle", file: 1, value: { backends: ["127.0.0.1:3"] }, toml: "" },
  ],
};

afterEach(() => vi.unstubAllGlobals());

function card(name: string) {
  return within(screen.getByRole("region", { name: `Pool ${name}` }));
}

test("a_runtime_pool_has_no_edit_button", async () => {
  serve({ pools: POOLS, "config/files": FILES });
  render(Pools);
  await screen.findByRole("button", { name: "Edit api" });
  expect(card("docker-web").getByText("runtime")).toBeTruthy();
  expect(card("docker-web").queryByRole("button", { name: /Edit/ })).toBeNull();
  expect(card("api").getByTitle("/etc/rxh/rxh.toml").textContent).toBe("rxh.toml");
  // A pool of the file that is not running can still be edited.
  expect(card("idle").getByRole("button", { name: "Edit idle" })).toBeTruthy();
});

test("health_and_state_are_text", async () => {
  serve({ pools: POOLS, "config/files": FILES });
  render(Pools);
  const row = (await screen.findByText("127.0.0.1:2")).closest("tr");
  expect(row?.textContent).toMatch(/down.*drain/s);
  const text = (address: string) => screen.getByText(address).closest("tr")?.textContent ?? "";
  expect(text("127.0.0.1:3")).toMatch(/3healthymaintenance/);
  expect(text("127.0.0.1:4")).toContain("healthy, at its limit");
  expect(card("api").getByRole("button", { name: /^Drain 127\.0\.0\.1:2/ }).getAttribute("aria-pressed")).toBe("true");
});

test("drain_calls_the_state_route_and_reads_the_pools_again", async () => {
  const fetch = serve({ pools: POOLS, "config/files": FILES });
  render(Pools);
  const drain = await screen.findByRole("button", { name: "Drain 10.0.0.5:80 until restart" });
  const before = fetch.mock.calls.filter(([url]) => url === "pools").length;
  await fireEvent.click(drain);
  expect(writes(fetch)).toEqual([
    { url: "pools/docker-web/upstreams/state", method: "PUT", body: { address: "10.0.0.5:80", state: "drain" } },
  ]);
  await vi.waitFor(() => expect(fetch.mock.calls.filter(([url]) => url === "pools").length).toBe(before + 1));
});

test("a_refused_state_change_is_reported", async () => {
  const fetch = serve({ pools: POOLS, "config/files": FILES }, { status: 404, body: { error: "no backend 10.0.0.5:80" } });
  render(Pools);
  await fireEvent.click(await screen.findByRole("button", { name: "Maintenance 10.0.0.5:80 until restart" }));
  expect((await screen.findByRole("alert")).textContent).toContain("no backend 10.0.0.5:80");
  // The reads that follow work: they must not take the refusal away.
  const reads = () => fetch.mock.calls.filter(([url]) => url === "pools").length;
  const before = reads();
  document.dispatchEvent(new Event("visibilitychange"));
  await vi.waitFor(() => expect(reads()).toBeGreaterThan(before));
  await new Promise((done) => setTimeout(done, 20));
  expect(screen.getByRole("alert").textContent).toContain("no backend 10.0.0.5:80");
});

test("without_a_configuration_file_nothing_can_be_edited", async () => {
  serve({ pools: POOLS, "config/files": { editable: false, files: [], entities: [] } });
  render(Pools);
  expect(await screen.findByText("No configuration file: read only")).toBeTruthy();
  expect(screen.queryByRole("button", { name: /Edit|New pool/ })).toBeNull();
  // The state of a backend is runtime: it needs no file.
  expect(screen.getByRole("button", { name: "Drain 10.0.0.5:80 until restart" })).toBeTruthy();
});

test("edit_opens_the_editor_and_a_save_reads_everything_again", async () => {
  const fetch = serve({ pools: POOLS, "config/files": FILES });
  render(Pools);
  await fireEvent.click(await screen.findByRole("button", { name: "Edit api" }));
  expect(screen.getByRole("heading", { name: "Pool api" })).toBeTruthy();
  await fireEvent.change(screen.getByLabelText("Policy"), { target: { value: "ip_hash" } });
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload" }));
  await screen.findByRole("button", { name: "Edit api" });
  expect(writes(fetch)).toEqual([
    {
      url: "config/entities/upstream/api",
      method: "PUT",
      body: { value: { backends: ["127.0.0.1:1", "127.0.0.1:2"], policy: "ip_hash" }, revision: "r1" },
    },
  ]);
  expect(fetch.mock.calls.filter(([url]) => url === "config/files").length).toBe(2);
});

test("a_new_pool_goes_to_the_chosen_file", async () => {
  const fetch = serve({ pools: POOLS, "config/files": FILES }, { status: 201, body: { key: "web" } });
  render(Pools);
  const file: HTMLSelectElement = await screen.findByLabelText("File of a new pool");
  expect(file.value).toBe("0");
  await fireEvent.change(file, { target: { value: "1" } });
  await fireEvent.click(screen.getByRole("button", { name: "New pool" }));
  await fireEvent.input(screen.getByLabelText("Name"), { target: { value: "web" } });
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload" }));
  await vi.waitFor(() => expect(writes(fetch)).toHaveLength(1));
  expect(writes(fetch)[0]).toEqual({ url: "config/entities/upstream", method: "POST", body: { value: {}, key: "web", file: 1 } });
});

test("the_files_are_read_again_once_a_token_is_given", async () => {
  // The first reads are made without the token and refused.
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({ error: "token" }, { status: 401 })));
  render(Pools);
  await screen.findAllByRole("alert");
  serve({ pools: POOLS, "config/files": FILES });
  setToken("0123456789abcdef");
  // Without the files the pool would read "runtime", with no way to edit it.
  expect(await screen.findByRole("button", { name: "Edit api" })).toBeTruthy();
  sessionStorage.clear();
});
