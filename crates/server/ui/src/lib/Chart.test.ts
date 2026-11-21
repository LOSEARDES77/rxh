import { render } from "@testing-library/svelte";
import { expect, test } from "vitest";
import Chart from "./Chart.svelte";

test('Chart shows "No data yet" for empty series', () => {
  const { container } = render(Chart, {
    series: [
      { label: "2xx", points: [] },
      { label: "5xx", points: [] },
    ],
    unit: "req/s",
  });
  expect(container.textContent).toContain("No data yet");
  expect(container.querySelector("polyline")).toBeNull();
});

test("Chart draws one labelled line per series and a table of the values", () => {
  const { container } = render(Chart, {
    series: [
      { label: "p50", points: [{ time: 0, value: 1 }, { time: 5, value: 3 }] },
      { label: "p99", points: [{ time: 5, value: 4 }] },
    ],
    unit: "ms",
  });
  expect(container.textContent).not.toContain("No data yet");
  expect(container.querySelector("svg title")?.textContent).toBe("p50, p99 (ms)");
  const lines = container.querySelectorAll("polyline");
  expect(lines).toHaveLength(2);
  // Time 0 at the left edge, the highest value (4) at the top.
  expect(lines[0].getAttribute("points")?.startsWith("0,")).toBe(true);
  expect(lines[1].getAttribute("points")).toMatch(/,0$/);
  const legend = [...container.querySelectorAll("li")].map((item) => item.textContent?.trim());
  expect(legend).toEqual(["p50", "p99"]);
  const rows = container.querySelectorAll("details tbody tr");
  expect(rows).toHaveLength(2);
  expect(rows[0].textContent).toContain("—");
});
