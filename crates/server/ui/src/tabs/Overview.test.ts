import { render, screen } from "@testing-library/svelte";
import { afterEach, expect, test, vi } from "vitest";
import { session } from "../lib/session";
import type { History, Sample } from "../lib/stats";
import Overview from "./Overview.svelte";
import { who } from "./serve";

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

/** `later` is what the daemon has sampled after the first answer. */
function serve(later: Sample[] = []) {
  const fetch = vi.fn(async (url: string) => {
    if (url === "stats/history") return Response.json(HISTORY);
    if (url.startsWith("stats/history?since=")) {
      const since = Number(url.split("=")[1]);
      return Response.json({ ...HISTORY, samples: later.filter((s) => s.time > since) });
    }
    return Response.json([{ listen: "127.0.0.1:443", subject: "CN=a", names: ["example.org"], days_left: 12 }]);
  });
  vi.stubGlobal("fetch", fetch);
  return fetch;
}

function asked(fetch: ReturnType<typeof serve>): string[] {
  return fetch.mock.calls.map(([url]) => url).filter((url) => url.startsWith("stats/"));
}

afterEach(() => {
  session.set(undefined);
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

test("later_polls_ask_for_the_new_samples_and_append_them", async () => {
  vi.useFakeTimers();
  const later = [sample(1010, 145, 5, 3)];
  const fetch = serve(later);
  render(Overview);
  await vi.advanceTimersByTimeAsync(0);
  expect(asked(fetch)).toEqual(["stats/history"]);
  expect(value("Requests per second")).toBe("10.0");

  await vi.advanceTimersByTimeAsync(5_000);
  expect(asked(fetch)).toEqual(["stats/history", "stats/history?since=1005"]);
  // 100 more requests in 5 seconds, counted from the sample the page kept.
  expect(value("Requests per second")).toBe("20.0");
  // Three samples give two rate points: a trend is drawn.
  expect(screen.getByText("Pool api").parentElement?.querySelector("svg")).toBeTruthy();

  // Nothing new: what the page has stays.
  await vi.advanceTimersByTimeAsync(5_000);
  expect(asked(fetch)[2]).toBe("stats/history?since=1010");
  expect(value("Requests per second")).toBe("20.0");

  // More than an hour later the old samples are gone.
  later.push(sample(1010 + 7200, 145, 5, 3), sample(1015 + 7200, 195, 5, 3));
  await vi.advanceTimersByTimeAsync(5_000);
  expect(asked(fetch)[3]).toBe("stats/history?since=1010");
  await vi.advanceTimersByTimeAsync(5_000);
  expect(asked(fetch)[4]).toBe(`stats/history?since=${1015 + 7200}`);
  expect(value("Requests per second")).toBe("10.0");
  expect(screen.getByText("Pool api").parentElement?.querySelector("svg")).toBeNull();
});

test("without_admin_no_pool_is_drawn_and_no_certificate_is_asked", async () => {
  session.set(who({ user: "luis", roles: ["web-editors"] }));
  const fetch = serve();
  render(Overview);
  expect(await screen.findByText("Server web")).toBeTruthy();
  expect(value("Requests per second")).toBe("10.0");
  expect(screen.queryByText("In flight")).toBeNull();
  expect(screen.queryByText("Healthy backends")).toBeNull();
  expect(screen.queryByText(/pool/i)).toBeNull();
  // `GET certificates` is an administrator's: asking would only show a refusal.
  expect(fetch.mock.calls.map(([url]) => url)).toEqual(["stats/history"]);
});
