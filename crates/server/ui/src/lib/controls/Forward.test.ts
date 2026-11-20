import { cleanup, fireEvent, screen } from "@testing-library/svelte";
import { expect, test } from "vitest";
import { checked, click, draw, field, type } from "./draw";

test("every_shape_is_shown_under_its_radio_and_left_as_it_was", () => {
  for (const [initial, radio] of [
    [undefined, "None"],
    ["@api", "Pool"],
    ["127.0.0.1:80", "Address"],
    ["{env.TARGET}", "Address"],
    [["a:1", "b:2"], "Several addresses"],
    [{ policy: "ip_hash", backends: ["127.0.0.1:8080"] }, "Advanced"],
    [[{ pool: "v1", weight: 90 }, { pool: "v2", weight: 10 }], "Advanced"],
    [["a:1", { address: "b:2", weight: 3 }], "Advanced"],
  ] as const) {
    const value = draw("forward", initial);
    expect(checked()).toBe(radio);
    expect(value()).toBe(initial);
    cleanup();
  }
});

test("a_pool_is_picked_from_the_pools_of_the_configuration", async () => {
  const value = draw("forward", "@api", ["api", "web"]);
  const select = field<HTMLSelectElement>("Pool");
  expect(select.value).toBe("@api");
  await fireEvent.change(select, { target: { value: "@web" } });
  expect(value()).toBe("@web");
});

test("a_pool_that_is_not_in_the_configuration_stays_selected", () => {
  draw("forward", "@{env.POOL}", ["api"]);
  expect(field<HTMLSelectElement>("Pool").value).toBe("@{env.POOL}");
});

test("each_radio_writes_one_shape", async () => {
  const value = draw("forward", "@api", ["api"]);
  await click("radio", "Address");
  expect(value()).toBeUndefined();
  await type("Address", "127.0.0.1:80");
  expect(value()).toBe("127.0.0.1:80");

  await click("radio", "Several addresses");
  expect(value()).toBeUndefined();
  await click("button", "Add address");
  await type("Address 1", "a:1");
  await click("button", "Add address");
  await type("Address 2", "b:2");
  expect(value()).toEqual(["a:1", "b:2"]);

  await click("radio", "Advanced");
  expect(value()).toEqual({});
  await click("button", "Add backend");
  await type("Backend 1", "c:3");
  expect(value()).toEqual({ backends: ["c:3"] });

  await click("radio", "Pool");
  expect(value()).toBeUndefined();
  await click("radio", "None");
  expect(value()).toBeUndefined();
});

test("a_weighted_split_is_edited_as_its_list", async () => {
  const value = draw("forward", [{ pool: "v1", weight: 90 }]);
  await type("Weight", "50");
  expect(value()).toEqual([{ pool: "v1", weight: 50 }]);
});

test("a_server_that_forwards_has_no_none_to_pick", async () => {
  // The Action choice of a server draws the control as required.
  const value = draw("forward", undefined, ["api"], true);
  expect(screen.queryByRole("radio", { name: "None" })).toBeNull();
  expect(checked()).toBe("Pool");
  await fireEvent.change(field("Pool"), { target: { value: "@api" } });
  expect(value()).toBe("@api");
});
