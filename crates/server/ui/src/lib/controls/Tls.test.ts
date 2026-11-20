import { cleanup, screen } from "@testing-library/svelte";
import { expect, test } from "vitest";
import { checked, click, draw, type } from "./draw";

test("the_three_radios_are_absent_auto_and_a_table", async () => {
  const value = draw("tls", undefined);
  expect(checked()).toBe("Off");
  await click("radio", "Automatic");
  expect(value()).toBe("auto");
  await click("radio", "Certificate files");
  expect(value()).toEqual({});
  await type("cert", "/etc/cert.pem");
  await type("key", "/etc/key.pem");
  expect(value()).toEqual({ cert: "/etc/cert.pem", key: "/etc/key.pem" });
  await click("radio", "Off");
  expect(value()).toBeUndefined();
  expect(screen.queryByRole("radio", { name: "As written" })).toBeNull();
});

test("every_shape_is_shown_under_its_radio_and_left_as_it_was", () => {
  for (const [initial, radio] of [
    ["auto", "Automatic"],
    [{ cert: "a.pem", key: "b.pem" }, "Certificate files"],
    ["{env.TLS}", "As written"],
  ] as const) {
    const value = draw("tls", initial);
    expect(checked()).toBe(radio);
    expect(value()).toBe(initial);
    cleanup();
  }
});

test("a_placeholder_is_shown_as_written_and_can_be_changed", async () => {
  const value = draw("tls", "{env.TLS}");
  expect((screen.getByLabelText("tls as written") as HTMLInputElement).value).toBe("{env.TLS}");
  await type("tls as written", "{env.MODE}");
  expect(value()).toBe("{env.MODE}");
});
