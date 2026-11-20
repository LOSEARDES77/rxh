import { render, screen } from "@testing-library/svelte";
import { afterEach, expect, test, vi } from "vitest";
import type { History, Sample } from "../lib/stats";
import Overview from "./Overview.svelte";

function sample(time: number, ok: number, failed: number, active: number): Sample {
  const buckets = Array(11).fill(0);
  buckets[4] = ok + failed;
  return {
    time,
    servers: {
      web: {
        status: [0, ok, 0, 0, failed],
        duration_count: ok + failed,
        duration_sum: 0,
        duration_buckets: buckets,
        connections: 0,
      },
    },
    pools: {
      api: {
        requests: ok + failed,
        failures: 0,
        responses: ok + failed,
        server_errors: failed,
        response_seconds: (ok + failed) * 0.02,
        active,
        healthy: 1,
        backends: 2,
      },
    },
  };
}

const HISTORY: History = {
  interval_seconds: 5,
  buckets: [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10],
  samples: [sample(1000, 0, 0, 0), sample(1005, 45, 5, 3)],
};

function serve() {
  const fetch = vi.fn(async (url: string) =>
    Response.json(url === "stats/history" ? HISTORY : [{ listen: "127.0.0.1:443", subject: "CN=a", names: ["example.org"], days_left: 12 }]),
  );
  vi.stubGlobal("fetch", fetch);
  return fetch;
}

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

function value(term: string): string | null | undefined {
  return screen.getByText(term, { selector: "dt" }).nextElementSibling?.textContent;
}

test("totals_rows_and_expiring_certificates", async () => {
  serve();
  render(Overview);
  expect(await screen.findByText("Pool api")).toBeTruthy();
  expect(value("Requests per second")).toBe("10.0");
  expect(value("Errors (5xx)")).toBe("10.0%");
  expect(value("In flight")).toBe("3");
  expect(value("Healthy backends")).toBe("1/2");
  expect(screen.getByText("Server web").parentElement?.textContent).toContain("10.0%");
  // Two samples give one rate point: too few for a trend.
  expect(screen.getByText("Pool api").parentElement?.lastElementChild?.textContent?.trim()).toBe("—");
  expect(screen.getByText(/example\.org on 127\.0\.0\.1:443/).textContent).toContain("12 d left");
});

test("no_requests_while_the_tab_is_hidden", async () => {
  vi.useFakeTimers();
  const fetch = serve();
  render(Overview);
  expect(fetch).toHaveBeenCalledTimes(2);

  vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
  document.dispatchEvent(new Event("visibilitychange"));
  await vi.advanceTimersByTimeAsync(20_000);
  expect(fetch).toHaveBeenCalledTimes(2);

  vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  document.dispatchEvent(new Event("visibilitychange"));
  expect(fetch).toHaveBeenCalledTimes(4);
  await vi.advanceTimersByTimeAsync(5_000);
  expect(fetch).toHaveBeenCalledTimes(6);
});
