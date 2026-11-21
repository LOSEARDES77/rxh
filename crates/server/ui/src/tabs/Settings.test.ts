import { fireEvent, render, screen, within } from "@testing-library/svelte";
import { get } from "svelte/store";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { setToken, stopped } from "../lib/api";
import Settings from "./Settings.svelte";
import { serve, writes } from "./serve";

// jsdom has no modal dialogs: enough of them for the page.
HTMLDialogElement.prototype.showModal = function () {
  this.open = true;
};
HTMLDialogElement.prototype.close = function () {
  this.open = false;
  this.dispatchEvent(new Event("close"));
};

const MAIN = "# main\n[log]\nformat = \"json\"\n";

const FILES = {
  editable: true,
  files: [
    { index: 0, path: "/etc/rxh/rxh.toml", revision: "r1", text: MAIN },
    { index: 1, path: "/etc/rxh/conf.d/extra.toml", revision: "r2", text: "" },
  ],
  entities: [
    { kind: "section", key: "log", file: 0, value: { format: "json" }, toml: "format = \"json\"\n" },
    { kind: "section", key: "admin", file: 0, value: { listen: "127.0.0.1:2019" }, toml: "" },
  ],
};

beforeEach(() => vi.stubGlobal("confirm", vi.fn(() => true)));
afterEach(() => {
  vi.unstubAllGlobals();
  stopped.set(false);
});

function section(label: string) {
  return within(screen.getByText(label, { selector: "th" }).closest("tr") as HTMLElement);
}

test("each_section_says_whether_it_is_in_the_file", async () => {
  serve({ "config/files": FILES });
  render(Settings);
  await screen.findByRole("button", { name: "Edit Access log" });
  expect(section("Access log").getByText("rxh.toml")).toBeTruthy();
  expect(section("General").getByText("not set")).toBeTruthy();
  expect(section("Kubernetes").getByText("[kubernetes]")).toBeTruthy();
  expect(section("Kubernetes").getByRole("button", { name: "Set up Kubernetes" })).toBeTruthy();
  // ACME lives in the certificates tab.
  expect(screen.queryByText("[acme]")).toBeNull();
});

test("a_section_missing_from_the_file_is_created_with_post", async () => {
  const fetch = serve({ "config/files": FILES }, { status: 201, body: { key: "tracing" } });
  render(Settings);
  await fireEvent.click(await screen.findByRole("button", { name: "Set up Tracing" }));
  expect(screen.getByRole("heading", { name: "New section tracing" })).toBeTruthy();
  // The name of a section is fixed: it is not asked for.
  expect(screen.queryByLabelText("Name")).toBeNull();
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload" }));
  await vi.waitFor(() => expect(writes(fetch)).toHaveLength(1));
  expect(writes(fetch)[0]).toEqual({
    url: "config/entities/section",
    method: "POST",
    body: { value: {}, key: "tracing", file: 0 },
  });
});

test("a_section_of_the_file_is_saved_with_put", async () => {
  const fetch = serve({ "config/files": FILES });
  render(Settings);
  await fireEvent.click(await screen.findByRole("button", { name: "Edit Access log" }));
  expect(screen.queryByText(/can lock this page out/)).toBeNull();
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload" }));
  await vi.waitFor(() => expect(writes(fetch)).toHaveLength(1));
  expect(writes(fetch)[0]).toMatchObject({ url: "config/entities/section/log", method: "PUT" });
});

test("the_admin_editor_warns_of_a_lock_out", async () => {
  serve({ "config/files": FILES });
  render(Settings);
  await fireEvent.click(await screen.findByRole("button", { name: "Edit Admin API" }));
  expect(
    screen.getByText(
      "Changing the address or the token takes effect on restart and can lock this page out.",
    ),
  ).toBeTruthy();
});

test("saving_a_file_sends_text_and_revision", async () => {
  const fetch = serve({ "config/files": FILES });
  render(Settings);
  const text: HTMLTextAreaElement = await screen.findByLabelText("/etc/rxh/rxh.toml");
  expect(text.value).toBe(MAIN);
  await fireEvent.input(text, { target: { value: `${MAIN}[tracing]\n` } });
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload /etc/rxh/rxh.toml" }));
  await vi.waitFor(() => expect(writes(fetch)).toHaveLength(1));
  expect(writes(fetch)[0]).toEqual({
    url: "config/files/0",
    method: "PUT",
    body: { text: `${MAIN}[tracing]\n`, revision: "r1" },
  });
});

test("a_refused_file_keeps_the_text_and_shows_the_message_as_it_comes", async () => {
  const message = "line 2: a secret of a [[server]] that moved\ntype the secret again";
  serve({ "config/files": FILES }, { status: 400, body: { error: message } });
  render(Settings);
  const text: HTMLTextAreaElement = await screen.findByLabelText("/etc/rxh/rxh.toml");
  await fireEvent.input(text, { target: { value: "typed" } });
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload /etc/rxh/rxh.toml" }));
  expect((await screen.findByRole("alert")).textContent).toContain(message);
  expect(text.value).toBe("typed");
});

test("a_changed_file_is_read_again_without_losing_the_typed_text", async () => {
  serve({ "config/files": FILES }, { status: 409, body: { error: "the file changed" } });
  render(Settings);
  const text: HTMLTextAreaElement = await screen.findByLabelText("/etc/rxh/rxh.toml");
  await fireEvent.input(text, { target: { value: "typed" } });
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload /etc/rxh/rxh.toml" }));
  await screen.findByText("the file changed");
  const changed = {
    ...FILES,
    files: [{ ...FILES.files[0], revision: "r9", text: "other" }, FILES.files[1]],
  };
  serve({ "config/files": changed });
  await fireEvent.click(screen.getByRole("button", { name: "Read again" }));
  const now: HTMLTextAreaElement = await screen.findByLabelText("What /etc/rxh/rxh.toml holds now");
  expect(now.value).toBe("other");
  expect(now.readOnly).toBe(true);
  expect(text.value).toBe("typed");
  // The next save is made over the revision just read.
  const again = serve({ "config/files": changed });
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload /etc/rxh/rxh.toml" }));
  await vi.waitFor(() => expect(writes(again)).toHaveLength(1));
  expect(writes(again)[0].body).toEqual({ text: "typed", revision: "r9" });
});

test("without_a_configuration_file_nothing_can_be_edited", async () => {
  serve({ "config/files": { editable: false, files: [], entities: [] } });
  render(Settings);
  expect(await screen.findByText("No configuration file: read only")).toBeTruthy();
  expect(screen.queryByRole("button", { name: /^(Edit|Set up) / })).toBeNull();
  // The daemon controls need no file.
  expect(screen.getByRole("button", { name: "Stop…" })).toBeTruthy();
});

test("reload_asks_first_and_reads_the_files_again", async () => {
  const fetch = serve({ "config/files": FILES }, { status: 200, body: { reloaded: true } });
  render(Settings);
  await fireEvent.click(await screen.findByRole("button", { name: "Reload" }));
  expect(confirm).toHaveBeenCalledWith(expect.stringContaining("Read the configuration files again"));
  await vi.waitFor(() => expect(writes(fetch)).toEqual([{ url: "reload", method: "POST", body: undefined }]));
  expect(await screen.findByRole("status")).toBeTruthy();
});

test("stop_is_disabled_until_the_word_is_typed", async () => {
  const fetch = serve({ "config/files": FILES }, { status: 200, body: { stopping: true } });
  render(Settings);
  await fireEvent.click(await screen.findByRole("button", { name: "Stop…" }));
  const dialog = within(screen.getByRole("dialog"));
  expect(dialog.getByText(/stops serving/)).toBeTruthy();
  const word = dialog.getByLabelText("Type stop to confirm");
  const go: HTMLButtonElement = dialog.getByRole("button", { name: "Stop rxh" });
  expect(go.disabled).toBe(true);
  await fireEvent.input(word, { target: { value: "stpo" } });
  expect(go.disabled).toBe(true);
  await fireEvent.click(go);
  expect(writes(fetch)).toEqual([]);
  await fireEvent.input(word, { target: { value: "stop" } });
  expect(go.disabled).toBe(false);
  await fireEvent.click(go);
  await vi.waitFor(() => expect(writes(fetch)).toEqual([{ url: "stop", method: "POST", body: undefined }]));
  await vi.waitFor(() => expect(get(stopped)).toBe(true));
});

test("cancel_or_escape_closes_the_dialog_without_acting", async () => {
  const fetch = serve({ "config/files": FILES });
  render(Settings);
  await fireEvent.click(await screen.findByRole("button", { name: "Upgrade…" }));
  const dialog = screen.getByRole("dialog") as HTMLDialogElement;
  expect(within(dialog).getByText(/binary on disk/)).toBeTruthy();
  await fireEvent.input(within(dialog).getByLabelText("Type upgrade to confirm"), { target: { value: "upgrade" } });
  await fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
  expect(dialog.open).toBe(false);
  // Opened again, the word must be typed again.
  await fireEvent.click(screen.getByRole("button", { name: "Upgrade…" }));
  expect((within(dialog).getByLabelText("Type upgrade to confirm") as HTMLInputElement).value).toBe("");
  // Escape: the browser sends `cancel`, then closes the dialog.
  await fireEvent(dialog, new Event("cancel", { cancelable: true }));
  dialog.close();
  expect(dialog.open).toBe(false);
  expect(writes(fetch)).toEqual([]);
});

test("a_refused_upgrade_is_reported", async () => {
  const fetch = serve({ "config/files": FILES }, { status: 400, body: { error: "upgrade needs Linux" } });
  render(Settings);
  await fireEvent.click(await screen.findByRole("button", { name: "Upgrade…" }));
  const dialog = within(screen.getByRole("dialog"));
  await fireEvent.input(dialog.getByLabelText("Type upgrade to confirm"), { target: { value: "upgrade" } });
  await fireEvent.click(dialog.getByRole("button", { name: "Upgrade rxh" }));
  expect((await screen.findByRole("alert")).textContent).toContain("upgrade needs Linux");
  expect(writes(fetch)).toEqual([{ url: "upgrade", method: "POST", body: undefined }]);
  expect(get(stopped)).toBe(false);
});

function disk(first: { text: string; revision: string }, second = { text: "", revision: "r2" }) {
  return {
    ...FILES,
    files: [
      { ...FILES.files[0], ...first },
      { ...FILES.files[1], ...second },
    ],
  };
}

test("saving_one_file_keeps_the_text_typed_in_another", async () => {
  const answers: Record<string, unknown> = { "config/files": FILES };
  serve(answers);
  render(Settings);
  const first: HTMLTextAreaElement = await screen.findByLabelText("/etc/rxh/rxh.toml");
  const second: HTMLTextAreaElement = screen.getByLabelText("/etc/rxh/conf.d/extra.toml");
  await fireEvent.input(second, { target: { value: "typed in extra" } });
  await fireEvent.input(first, { target: { value: "saved text" } });
  // The disk as the daemon writes it back: the file just saved takes it.
  answers["config/files"] = disk({ text: "saved text\n", revision: "r3" });
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload /etc/rxh/rxh.toml" }));
  await vi.waitFor(() => expect(first.value).toBe("saved text\n"));
  expect(second.value).toBe("typed in extra");
  expect(screen.queryByRole("alert")).toBeNull();
});

test("a_section_save_keeps_the_typed_text_and_its_save_is_made_over_the_old_revision", async () => {
  const answers: Record<string, unknown> = { "config/files": FILES };
  const write = { status: 200, body: {} as unknown };
  const fetch = serve(answers, write);
  render(Settings);
  await fireEvent.input(await screen.findByLabelText("/etc/rxh/rxh.toml"), { target: { value: "typed" } });
  await fireEvent.click(screen.getByRole("button", { name: "Edit Access log" }));
  answers["config/files"] = disk({ text: "# main\n[log]\nformat = \"text\"\n", revision: "r5" });
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload" }));
  const text: HTMLTextAreaElement = await screen.findByLabelText("/etc/rxh/rxh.toml");
  expect(text.value).toBe("typed");
  // The page says the disk moved under the draft, before any save is tried.
  expect((await screen.findByRole("alert")).textContent).toContain("changed on disk");
  Object.assign(write, { status: 409, body: { error: "the file changed since it was read" } });
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload /etc/rxh/rxh.toml" }));
  await vi.waitFor(() => expect(writes(fetch)).toHaveLength(2));
  // Over the revision the text was typed on: the daemon refuses it with 409.
  expect(writes(fetch)[1].body).toEqual({ text: "typed", revision: "r1" });
  await screen.findByText("the file changed since it was read");
  Object.assign(write, { status: 200, body: {} });
  await fireEvent.click(screen.getByRole("button", { name: "Read again" }));
  const now: HTMLTextAreaElement = await screen.findByLabelText("What /etc/rxh/rxh.toml holds now");
  expect(now.value).toContain("format = \"text\"");
  expect(text.value).toBe("typed");
  expect(screen.queryByRole("alert")).toBeNull();
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload /etc/rxh/rxh.toml" }));
  await vi.waitFor(() => expect(writes(fetch)).toHaveLength(3));
  expect(writes(fetch)[2].body).toEqual({ text: "typed", revision: "r5" });
});

test("reload_keeps_the_typed_text_and_moves_the_untouched_files", async () => {
  const answers: Record<string, unknown> = { "config/files": FILES };
  serve(answers, { status: 200, body: { reloaded: true } });
  render(Settings);
  const first: HTMLTextAreaElement = await screen.findByLabelText("/etc/rxh/rxh.toml");
  const second: HTMLTextAreaElement = screen.getByLabelText("/etc/rxh/conf.d/extra.toml");
  await fireEvent.input(first, { target: { value: "typed" } });
  answers["config/files"] = disk({ text: MAIN, revision: "r1" }, { text: "changed by hand", revision: "r7" });
  await fireEvent.click(screen.getByRole("button", { name: "Reload" }));
  await vi.waitFor(() => expect(second.value).toBe("changed by hand"));
  expect(first.value).toBe("typed");
  // Its file did not change: nothing to warn about.
  expect(screen.queryByRole("alert")).toBeNull();
});

test("the_files_are_read_again_once_a_token_is_given", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({ error: "token" }, { status: 401 })));
  render(Settings);
  await screen.findByRole("alert");
  serve({ "config/files": FILES });
  setToken("0123456789abcdef");
  expect(await screen.findByRole("button", { name: "Edit Access log" })).toBeTruthy();
  sessionStorage.clear();
});
