import { cleanup, screen } from "@testing-library/svelte";
import { expect, test } from "vitest";
import { click, draw, type } from "./draw";

test("text_and_table_are_left_as_they_were", () => {
  for (const initial of ["/var/www", "{env.X}", { root: "/var/www", browse: "{env.Y}" }]) {
    const value = draw("serve", initial);
    expect(value()).toBe(initial);
    cleanup();
  }
});

test("the_text_is_the_value_until_more_options_are_asked_for", async () => {
  const value = draw("serve", undefined);
  await type("Directory", "/var/www");
  expect(value()).toBe("/var/www");
  await click("button", "More serve options");
  expect(value()).toEqual({ root: "/var/www" });
  expect((screen.getByLabelText("Root") as HTMLInputElement).value).toBe("/var/www");
  expect(screen.getByLabelText("Browse")).toBeTruthy();
  await click("button", "Only the directory (drops the other serve options)");
  expect(value()).toBe("/var/www");
  await type("Directory", "");
  expect(value()).toBeUndefined();
});
