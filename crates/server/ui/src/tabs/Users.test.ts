import { fireEvent, render, screen, within } from "@testing-library/svelte";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { session } from "../lib/session";
import Users from "./Users.svelte";
import { serve, who, writes } from "./serve";

const USERS = [
  { name: "ana", roles: ["admin"] },
  { name: "luis", roles: ["web-editors"] },
];
const ROLES = [
  { name: "admin", builtin: true },
  {
    name: "web-editors",
    grant: [
      { group: "web", allow: ["view", "modify", "create"] },
      { host: "intranet", allow: ["view"] },
    ],
  },
];
const FILES = {
  editable: true,
  files: [{ index: 0, path: "/etc/rxh/rxh.toml", revision: "r1", text: "" }],
  entities: [{ kind: "server", key: "0:0", file: 0, value: { name: "shop", groups: ["web", "store"] }, toml: "" }],
};
const ANSWERS = { users: USERS, roles: ROLES, "config/files": FILES };

let confirm: ReturnType<typeof vi.fn<(message?: string) => boolean>>;

beforeEach(() => {
  session.set(who({ user: "ana", admin: true, roles: ["admin"] }));
  confirm = vi.fn(() => true);
  vi.stubGlobal("confirm", confirm);
});

afterEach(() => vi.unstubAllGlobals());

function row(name: string) {
  return within(screen.getByRole("rowheader", { name }).closest("tr") as HTMLElement);
}

function type(label: string, text: string) {
  return fireEvent.input(screen.getByLabelText(label), { target: { value: text } });
}

function box(label: string): HTMLInputElement {
  return screen.getByRole("checkbox", { name: label });
}

test("users_and_roles_are_listed", async () => {
  serve(ANSWERS);
  render(Users);
  await screen.findByRole("rowheader", { name: "luis" });
  expect((row("luis").getByRole("checkbox", { name: "web-editors for luis" }) as HTMLInputElement).checked).toBe(true);
  expect((row("luis").getByRole("checkbox", { name: "admin for luis" }) as HTMLInputElement).checked).toBe(false);
  expect((row("ana").getByRole("checkbox", { name: "admin for ana" }) as HTMLInputElement).checked).toBe(true);
  expect(row("ana").getByRole("button", { name: "Set password for ana" })).toBeTruthy();
  expect(row("luis").getByRole("button", { name: "Remove luis" })).toBeTruthy();

  const roles = within(screen.getByRole("region", { name: "Roles" }));
  // The built-in role cannot be changed or removed.
  expect(roles.getByText("built in")).toBeTruthy();
  expect(roles.queryByRole("button", { name: "Edit admin" })).toBeNull();
  expect(roles.queryByRole("button", { name: "Remove admin" })).toBeNull();
  const editors = roles.getByRole("rowheader", { name: "web-editors" }).closest("tr")?.textContent ?? "";
  expect(editors).toContain("group web: view, modify, create");
  expect(editors).toContain("host intranet: view");
  expect(roles.getByRole("button", { name: "Edit web-editors" })).toBeTruthy();
});

test("a_new_user_is_posted_with_its_roles", async () => {
  const fetch = serve(ANSWERS, { status: 201, body: { name: "maria", roles: ["web-editors"] } });
  render(Users);
  await fireEvent.click(await screen.findByRole("button", { name: "New user" }));
  const form = within(screen.getByRole("form", { name: "New user" }));
  await type("Name", "maria");
  await type("Password", "correct horse");
  await type("Repeat the password", "correct horse");
  await fireEvent.click(form.getByRole("checkbox", { name: "web-editors" }));
  await fireEvent.click(form.getByRole("button", { name: "Add user" }));
  await vi.waitFor(() => expect(screen.queryByRole("form", { name: "New user" })).toBeNull());
  expect(writes(fetch)).toEqual([
    { url: "users", method: "POST", body: { name: "maria", password: "correct horse", roles: ["web-editors"] } },
  ]);
});

test("a_grant_row_ticks_view_with_another_box", async () => {
  const fetch = serve(ANSWERS, { status: 201, body: {} });
  render(Users);
  await fireEvent.click(await screen.findByRole("button", { name: "New role" }));
  await type("Role name", "shop-team");
  await fireEvent.click(screen.getByRole("button", { name: "Add grant" }));
  // The groups of the servers are offered; any other may be typed.
  const target = screen.getByLabelText("Target of grant 1") as HTMLInputElement;
  const offered = [...(document.getElementById(target.getAttribute("list") ?? "")?.children ?? [])];
  expect(offered.map((option) => (option as HTMLOptionElement).value)).toEqual(["store", "web"]);
  await type("Target of grant 1", "web");
  expect(box("View (grant 1)").checked).toBe(false);
  await fireEvent.click(box("Delete (grant 1)"));
  expect(box("View (grant 1)").checked).toBe(true);
  await fireEvent.click(screen.getByRole("button", { name: "Save role" }));
  await vi.waitFor(() => expect(writes(fetch)).toHaveLength(1));
  expect(writes(fetch)[0]).toEqual({
    url: "roles",
    method: "POST",
    body: { name: "shop-team", grant: [{ group: "web", allow: ["view", "delete"] }] },
  });
});

test("create_is_disabled_on_a_host_grant", async () => {
  const fetch = serve(ANSWERS, { status: 204, body: null });
  render(Users);
  await fireEvent.click(await screen.findByRole("button", { name: "Edit web-editors" }));
  expect(box("Create (grant 1)").disabled).toBe(false);
  expect(box("Create (grant 2)").disabled).toBe(true);
  // Host names are offered on a host row.
  const target = screen.getByLabelText("Target of grant 2") as HTMLInputElement;
  const offered = [...(document.getElementById(target.getAttribute("list") ?? "")?.children ?? [])];
  expect(offered.map((option) => (option as HTMLOptionElement).value)).toEqual(["shop"]);

  // A group row made a host row loses `create`.
  await fireEvent.change(screen.getByLabelText("Kind of grant 1"), { target: { value: "host" } });
  expect(box("Create (grant 1)").disabled).toBe(true);
  expect(box("Create (grant 1)").checked).toBe(false);
  await fireEvent.click(screen.getByRole("button", { name: "Save role" }));
  await vi.waitFor(() => expect(writes(fetch)).toHaveLength(1));
  expect(writes(fetch)[0]).toEqual({
    url: "roles/web-editors",
    method: "PUT",
    body: {
      grant: [
        { host: "web", allow: ["view", "modify"] },
        { host: "intranet", allow: ["view"] },
      ],
    },
  });
});

test("a_refusal_is_shown_and_the_form_kept", async () => {
  serve(ANSWERS, { status: 409, body: { error: 'role "shop" is not defined' } });
  render(Users);
  await fireEvent.click(await screen.findByRole("button", { name: "New user" }));
  await type("Name", "maria");
  await type("Password", "correct horse");
  await type("Repeat the password", "correct horse");
  await fireEvent.click(screen.getByRole("button", { name: "Add user" }));
  const form = within(screen.getByRole("form", { name: "New user" }));
  expect((await form.findByRole("alert")).textContent).toBe('role "shop" is not defined');
  expect((screen.getByLabelText("Name") as HTMLInputElement).value).toBe("maria");
  expect((screen.getByLabelText("Password") as HTMLInputElement).value).toBe("correct horse");

  // Next to the row it is about, and the change typed there stays.
  await fireEvent.click(row("luis").getByRole("checkbox", { name: "admin for luis" }));
  await fireEvent.click(row("luis").getByRole("button", { name: "Save roles of luis" }));
  const luis = screen.getByRole("rowheader", { name: "luis" }).closest("tr") as HTMLElement;
  await vi.waitFor(() => expect(luis.nextElementSibling?.textContent).toBe('role "shop" is not defined'));
  expect((row("luis").getByRole("checkbox", { name: "admin for luis" }) as HTMLInputElement).checked).toBe(true);
});

test("removing_asks_first", async () => {
  const fetch = serve(ANSWERS, { status: 204, body: null });
  render(Users);
  confirm.mockReturnValue(false);
  await fireEvent.click(await screen.findByRole("button", { name: "Remove luis" }));
  await fireEvent.click(screen.getByRole("button", { name: "Remove web-editors" }));
  expect(confirm).toHaveBeenCalledTimes(2);
  expect(confirm.mock.calls[0][0]).toContain("luis");
  expect(confirm.mock.calls[1][0]).toContain("web-editors");
  expect(writes(fetch)).toEqual([]);

  confirm.mockReturnValue(true);
  await fireEvent.click(screen.getByRole("button", { name: "Remove luis" }));
  await fireEvent.click(screen.getByRole("button", { name: "Remove web-editors" }));
  await vi.waitFor(() => expect(writes(fetch)).toHaveLength(2));
  expect(writes(fetch)).toEqual([
    { url: "users/luis", method: "DELETE", body: undefined },
    { url: "roles/web-editors", method: "DELETE", body: undefined },
  ]);
});

test("taking_admin_from_oneself_asks_first", async () => {
  const fetch = serve({ ...ANSWERS, "auth/session": who({ user: "ana" }) }, { status: 204, body: null });
  render(Users);
  await fireEvent.click(await screen.findByRole("button", { name: "Remove luis" }));
  await vi.waitFor(() => expect(writes(fetch)).toHaveLength(1));
  // Another user's removal says nothing of losing the tab.
  expect(confirm.mock.calls[0][0]).not.toContain("this tab");

  confirm.mockReturnValue(false);
  await fireEvent.click(row("ana").getByRole("checkbox", { name: "admin for ana" }));
  await fireEvent.click(row("ana").getByRole("button", { name: "Save roles of ana" }));
  await fireEvent.click(row("ana").getByRole("button", { name: "Remove ana" }));
  expect(confirm.mock.calls[1][0]).toContain("this tab");
  expect(confirm.mock.calls[2][0]).toContain("this tab");
  expect(writes(fetch)).toHaveLength(1);

  // Asked and accepted, the page asks who it is for at once.
  confirm.mockReturnValue(true);
  await fireEvent.click(row("ana").getByRole("button", { name: "Save roles of ana" }));
  await vi.waitFor(() => expect(writes(fetch)).toHaveLength(2));
  expect(writes(fetch)[1]).toEqual({ url: "users/ana", method: "PUT", body: { roles: [] } });
  await vi.waitFor(() => expect(fetch.mock.calls.some(([url]) => url === "auth/session")).toBe(true));
});

test("setting_a_password_sends_it_alone", async () => {
  const fetch = serve(ANSWERS, { status: 204, body: null });
  render(Users);
  await fireEvent.click(await screen.findByRole("button", { name: "Set password for luis" }));
  await type("New password", "battery staple");
  await type("Repeat the new password", "battery staple");
  await fireEvent.click(screen.getByRole("button", { name: "Set password" }));
  await vi.waitFor(() => expect(writes(fetch)).toHaveLength(1));
  expect(writes(fetch)[0]).toEqual({ url: "users/luis", method: "PUT", body: { password: "battery staple" } });
});

test("unticking_view_unticks_the_others", async () => {
  const fetch = serve(ANSWERS, { status: 204, body: null });
  render(Users);
  await fireEvent.click(await screen.findByRole("button", { name: "Edit web-editors" }));
  // Modify, Create or Delete without View would be a grant that does nothing.
  await fireEvent.click(box("View (grant 1)"));
  expect(["View", "Modify", "Create", "Delete"].map((label) => box(`${label} (grant 1)`).checked)).toEqual([false, false, false, false]);
  await fireEvent.click(screen.getByRole("button", { name: "Save role" }));
  await vi.waitFor(() => expect(writes(fetch)).toHaveLength(1));
  expect(writes(fetch)[0].body.grant[0]).toEqual({ group: "web", allow: [] });
});

test("focus_follows_adding_and_removing", async () => {
  // A local administrator: removing a user is not removing itself.
  session.set(who({ admin: true }));
  const answers = { ...ANSWERS };
  serve(answers, { status: 204, body: null });
  render(Users);
  await fireEvent.click(await screen.findByRole("button", { name: "Edit web-editors" }));
  await fireEvent.click(screen.getByRole("button", { name: "Add grant" }));
  await vi.waitFor(() => expect(document.activeElement).toBe(screen.getByLabelText("Target of grant 3")));
  // The next row takes the place of the removed one, then the row before, then "Add grant".
  await fireEvent.click(screen.getByRole("button", { name: "Remove grant 1" }));
  await vi.waitFor(() => expect(document.activeElement).toBe(screen.getByLabelText("Target of grant 1")));
  expect((document.activeElement as HTMLInputElement).value).toBe("intranet");
  await fireEvent.click(screen.getByRole("button", { name: "Remove grant 2" }));
  await vi.waitFor(() => expect(document.activeElement).toBe(screen.getByLabelText("Target of grant 1")));
  await fireEvent.click(screen.getByRole("button", { name: "Remove grant 1" }));
  await vi.waitFor(() => expect(document.activeElement).toBe(screen.getByRole("button", { name: "Add grant" })));
  await fireEvent.click(screen.getByRole("button", { name: "Cancel" }));

  // A removed user's button goes with the list read again: the next row's takes focus, then the toolbar's.
  answers.users = [USERS[1]];
  await fireEvent.click(screen.getByRole("button", { name: "Remove ana" }));
  await vi.waitFor(() => expect(document.activeElement).toBe(screen.getByRole("button", { name: "Remove luis" })));
  answers.users = [];
  await fireEvent.click(screen.getByRole("button", { name: "Remove luis" }));
  await vi.waitFor(() => expect(document.activeElement).toBe(screen.getByRole("button", { name: "New user" })));
  answers.roles = [ROLES[0]];
  await fireEvent.click(screen.getByRole("button", { name: "Remove web-editors" }));
  await vi.waitFor(() => expect(document.activeElement).toBe(screen.getByRole("button", { name: "New role" })));
});

test("the_users_tab_explains_provider_users", async () => {
  session.set(who({ user: "ana", admin: true, roles: ["admin"], oidc: true, oidc_issuer: "id.example.com" }));
  serve(ANSWERS);
  const on = render(Users);
  await screen.findByRole("rowheader", { name: "luis" });
  const note = screen.getByText(/People who sign in through id\.example\.com are not listed here/);
  expect(note.textContent).toBe(
    "People who sign in through id.example.com are not listed here; their roles come from [admin.oidc.roles] in the configuration.",
  );
  on.unmount();

  session.set(who({ user: "ana", admin: true, roles: ["admin"] }));
  render(Users);
  await screen.findByRole("rowheader", { name: "luis" });
  expect(screen.queryByText(/People who sign in through/)).toBeNull();
});
