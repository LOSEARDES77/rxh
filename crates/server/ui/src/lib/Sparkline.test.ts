import { render } from "@testing-library/svelte";
import { expect, test } from "vitest";
import Sparkline from "./Sparkline.svelte";

test("a_row_with_points_renders_a_named_sparkline", () => {
  const { container } = render(Sparkline, {
    points: [
      { time: 0, value: 2 },
      { time: 5, value: 4 },
      { time: 10, value: 3 },
    ],
  });
  const svg = container.querySelector("svg");
  expect(svg?.getAttribute("aria-label")).toBe("Requests per second trend, last value 3.0");
  // Highest value at the top, time from left to right.
  expect(svg?.querySelector("polyline")?.getAttribute("points")).toBe("0,10 40,0 80,5");
});

test("a_row_without_points_renders_a_dash", () => {
  for (const points of [[], [{ time: 0, value: 1 }]]) {
    const { container, unmount } = render(Sparkline, { points });
    expect(container.querySelector("svg")).toBeNull();
    expect(container.textContent?.trim()).toBe("—");
    unmount();
  }
});
