import { cleanup, screen } from "@testing-library/svelte";
import { expect, test } from "vitest";
import { click, draw, type } from "./draw";

function input(label: string): HTMLInputElement {
  return screen.getByLabelText(label);
}

test("every_shape_is_shown_in_rows_and_left_as_it_was", () => {
  for (const initial of [
    undefined,
    ["127.0.0.1:1", "{env.B}:80"],
    [{ address: "127.0.0.1:1" }, { address: "127.0.0.1:2", weight: 3 }],
    ["127.0.0.1:1", { address: "{env.B}", weight: "{env.W}", zone: "eu" }],
    [],
  ]) {
    const value = draw("backends", initial);
    expect(value()).toBe(initial);
    expect(screen.queryByLabelText("Written as")).toBeNull();
    expect(screen.queryAllByRole("button", { name: /^Remove backend/ })).toHaveLength(initial?.length ?? 0);
    cleanup();
  }
});

test("a_row_shows_the_address_and_the_weight", () => {
  draw("backends", ["127.0.0.1:1", { address: "{env.B}", weight: "{env.W}" }, { address: "c:3", weight: 2 }]);
  expect(input("Backend 1").value).toBe("127.0.0.1:1");
  expect(input("Weight of backend 1").value).toBe("");
  expect(input("Backend 2").value).toBe("{env.B}");
  expect(input("Weight of backend 2").value).toBe("{env.W}");
  expect(input("Weight of backend 3").value).toBe("2");
});

test("a_text_stays_a_text_and_a_table_stays_a_table", async () => {
  const value = draw("backends", ["a:1", { address: "b:2" }]);
  await type("Backend 1", "a:9");
  await type("Backend 2", "b:9");
  expect(value()).toEqual(["a:9", { address: "b:9" }]);
  // A weight needs the table; taking it away leaves the table.
  await type("Weight of backend 1", "5");
  expect(value()).toEqual([{ address: "a:9", weight: 5 }, { address: "b:9" }]);
  await type("Weight of backend 1", "");
  expect(value()).toEqual([{ address: "a:9" }, { address: "b:9" }]);
  await type("Weight of backend 2", "");
  expect(value()).toEqual([{ address: "a:9" }, { address: "b:9" }]);
});

test("rows_are_added_and_removed", async () => {
  const value = draw("backends", undefined);
  await click("button", "Add backend");
  await type("Backend 1", "a:1");
  await click("button", "Add backend");
  await type("Backend 2", "b:2");
  expect(value()).toEqual(["a:1", "b:2"]);
  await click("button", "Remove backend 1");
  expect(value()).toEqual(["b:2"]);
  expect(input("Backend 1").value).toBe("b:2");
  await click("button", "Remove backend 1");
  expect(value()).toBeUndefined();
});

test("the_other_keys_of_a_backend_are_kept_behind_more_options", async () => {
  const value = draw("backends", [{ address: "a:1", zone: "eu" }]);
  expect(screen.getByText("More options of backend 1")).toBeTruthy();
  await type("Backend 1", "a:2");
  expect(value()).toEqual([{ address: "a:2", zone: "eu" }]);
  await type("zone (JSON)", '"us"');
  expect(value()).toEqual([{ address: "a:2", zone: "us" }]);
});
