import { expect, test } from "vitest";
import { append, last, quantile, rate, window, type History, type Sample, type ServerCounters } from "./stats";

const BUCKETS = [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10];

function server(fields: Partial<ServerCounters> = {}): ServerCounters {
  return {
    status: [0, 0, 0, 0, 0],
    duration_count: 0,
    duration_sum: 0,
    duration_buckets: Array(11).fill(0),
    connections: 0,
    ...fields,
  };
}

function sample(time: number, servers: Record<string, ServerCounters> = {}): Sample {
  return { time, servers, pools: {} };
}

function ok(samples: Sample[]) {
  return rate(samples, (s) => s.servers.web?.status[1]);
}

test("rate_is_the_difference_over_the_interval", () => {
  const samples = [0, 50, 150].map((ok, i) => sample(i * 5, { web: server({ status: [0, ok, 0, 0, 0] }) }));
  expect(ok(samples)).toEqual([
    { time: 5, value: 10 },
    { time: 10, value: 20 },
  ]);
});

test("a_counter_that_goes_back_is_skipped", () => {
  const samples = [100, 20, 70].map((ok, i) => sample(i * 5, { web: server({ status: [0, ok, 0, 0, 0] }) }));
  expect(ok(samples)).toEqual([{ time: 10, value: 10 }]);
});

test("a_series_missing_in_one_sample_is_skipped", () => {
  const samples = [
    sample(0, { web: server({ status: [0, 10, 0, 0, 0] }) }),
    sample(5),
    sample(10, { web: server({ status: [0, 30, 0, 0, 0] }) }),
    sample(15, { web: server({ status: [0, 40, 0, 0, 0] }) }),
  ];
  expect(ok(samples)).toEqual([{ time: 15, value: 2 }]);
});

test("no_samples_and_one_sample_give_no_points", () => {
  expect(ok([])).toEqual([]);
  expect(ok([sample(0, { web: server() })])).toEqual([]);
  expect(quantile([], null, 0.5, BUCKETS)).toEqual([]);
  expect(quantile([sample(0, { web: server() })], null, 0.5, BUCKETS)).toEqual([]);
  expect(last([])).toBeNull();
});

test("no_point_is_ever_nan_or_negative", () => {
  const pick = (max: number) => Math.floor(Math.random() * max);
  for (let round = 0; round < 200; round++) {
    const samples: Sample[] = [];
    let time = 0;
    for (let i = 0; i < 20; i++) {
      // Times may repeat; counters jump back and forth; servers come and go.
      time += pick(3) * 5;
      const servers: Record<string, ServerCounters> = {};
      for (const name of ["a", "b"]) {
        if (pick(4) === 0) continue;
        const buckets = Array.from({ length: 11 }, () => pick(50));
        const count = buckets.reduce((sum, n) => sum + n, 0) + pick(5);
        servers[name] = server({
          status: [pick(9), pick(99), pick(9), pick(9), pick(9)],
          duration_count: count,
          duration_buckets: buckets,
        });
      }
      samples.push(sample(time, servers));
    }
    const points = [
      ...rate(samples, (s) => s.servers.a?.status[1]),
      ...quantile(samples, null, Math.random(), BUCKETS),
      ...quantile(samples, "b", 0.99, BUCKETS),
    ];
    for (const point of points) {
      expect(Number.isFinite(point.value)).toBe(true);
      expect(point.value).toBeGreaterThanOrEqual(0);
    }
  }
});

test("quantile_interpolates_in_the_bucket", () => {
  const buckets = Array(11).fill(0);
  buckets[4] = 100;
  const samples = [
    sample(0, { web: server() }),
    sample(5, { web: server({ duration_count: 100, duration_buckets: buckets }) }),
  ];
  const [median] = quantile(samples, "web", 0.5, BUCKETS);
  expect(median.time).toBe(5);
  expect(median.value).toBeCloseTo(0.075, 12);
  // `null` sums the servers: a second, idle server changes nothing.
  samples[0].servers.idle = server();
  samples[1].servers.idle = server();
  expect(quantile(samples, null, 0.5, BUCKETS)[0].value).toBeCloseTo(0.075, 12);
  // No request in the pair: no point.
  expect(quantile([samples[1], { ...samples[1], time: 10 }], null, 0.5, BUCKETS)).toEqual([]);
});

test("a_quantile_above_the_last_bound_is_the_last_bound", () => {
  const samples = [sample(0, { web: server() }), sample(5, { web: server({ duration_count: 10 }) })];
  expect(quantile(samples, "web", 0.5, BUCKETS)).toEqual([{ time: 5, value: 10 }]);
});

test("window_cuts_by_time", () => {
  const samples = Array.from({ length: 721 }, (_, i) => sample(1000 + i * 5));
  const recent = window(samples, 300);
  expect(recent).toHaveLength(61);
  expect(recent[0].time).toBe(1000 + 3600 - 300);
  expect(window(samples, 3600)).toHaveLength(721);
  expect(window([], 900)).toEqual([]);
});

test("last_is_the_newest_value", () => {
  expect(last([{ time: 1, value: 3 }, { time: 2, value: 4 }])).toBe(4);
});

function history(times: number[]): History {
  return { interval_seconds: 5, buckets: BUCKETS, samples: times.map((time) => sample(time)) };
}

function times(history: History): number[] {
  return history.samples.map((s) => s.time);
}

test("append_adds_the_newer_samples_after_the_ones_kept", () => {
  expect(times(append(undefined, history([0, 5])))).toEqual([0, 5]);
  expect(times(append(history([0, 5]), history([10, 15])))).toEqual([0, 5, 10, 15]);
  expect(times(append(history([0, 5]), history([])))).toEqual([0, 5]);
  // Two answers to the same question, or a daemon asked without `since`.
  expect(times(append(history([0, 5, 10]), history([5, 10, 15])))).toEqual([0, 5, 10, 15]);
});

test("append_keeps_one_hour", () => {
  const kept = history([0, 5, 10, 3595]);
  expect(times(append(kept, history([3600, 3605])))).toEqual([5, 10, 3595, 3600, 3605]);
  expect(times(append(kept, history([9000])))).toEqual([9000]);
});
