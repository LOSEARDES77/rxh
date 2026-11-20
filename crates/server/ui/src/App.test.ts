import { render, waitFor } from "@testing-library/svelte";
import { afterEach, expect, test, vi } from "vitest";
import App from "./App.svelte";

afterEach(() => vi.unstubAllGlobals());

// jsdom has no modal dialogs: enough of them for the page.
HTMLDialogElement.prototype.showModal = function () {
  this.open = true;
};
HTMLDialogElement.prototype.close = function () {
  this.open = false;
  this.dispatchEvent(new Event("close"));
};

test("the_token_dialog_stays_open_until_a_token_is_given", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => Response.json({ error: "admin API requires a token" }, { status: 401 })),
  );
  const { container } = render(App);
  const dialog = container.ownerDocument.querySelector("dialog") as HTMLDialogElement;
  await waitFor(() => expect(dialog.open).toBe(true));

  // Escape fires `cancel`; closing anyway would leave the page without a way to give the token.
  const cancel = new Event("cancel", { cancelable: true });
  dialog.dispatchEvent(cancel);
  expect(cancel.defaultPrevented).toBe(true);
  dialog.close();
  await waitFor(() => expect(dialog.open).toBe(true));
});
