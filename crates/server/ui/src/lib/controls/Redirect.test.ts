import { cleanup, screen } from "@testing-library/svelte";
import { expect, test } from "vitest";
import { click, draw, type } from "./draw";

test("text_and_table_are_left_as_they_were", () => {
  for (const initial of ["https://example.org{uri}", "{env.X}", { to: "https://example.org{uri}", status: "{env.Y}" }]) {
    const value = draw("redirect", initial);
    expect(value()).toBe(initial);
    cleanup();
  }
});

test("the_text_is_the_value_until_more_options_are_asked_for", async () => {
  const value = draw("redirect", undefined);
  await type("Target", "https://example.org{uri}");
  expect(value()).toBe("https://example.org{uri}");
  await click("button", "More redirect options");
  expect(value()).toEqual({ to: "https://example.org{uri}" });
  expect((screen.getByLabelText("To") as HTMLInputElement).value).toBe("https://example.org{uri}");
  expect(screen.getByLabelText("Status")).toBeTruthy();
  await click("button", "Only the target (drops the other redirect options)");
  expect(value()).toBe("https://example.org{uri}");
  await type("Target", "");
  expect(value()).toBeUndefined();
});
