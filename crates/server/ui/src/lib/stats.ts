// Turns the cumulative counters of `GET stats/history` into points to draw.

/** Counters of one server (`ServerCounters` in `metrics.rs`). */
export interface ServerCounters {
  /** HTTP responses by status class, 1xx to 5xx. */
  status: number[];
  duration_count: number;
  duration_sum: number;
  /** Requests per duration bucket, not cumulative; slower ones are in `duration_count` only. */
  duration_buckets: number[];
  /** Layer 4 connections and UDP sessions. */
  connections: number;
}

/** Counters of one pool (`PoolCounters` in `history.rs`); the last three are gauges. */
export interface PoolCounters {
  requests: number;
  failures: number;
  responses: number;
  server_errors: number;
  response_seconds: number;
  active: number;
  healthy: number;
  backends: number;
}

/** Every counter at one point in time. */
export interface Sample {
  /** Unix time in seconds. */
  time: number;
  servers: Record<string, ServerCounters>;
  /** Empty while no daemon serves the admin API. */
  pools: Record<string, PoolCounters>;
}

export interface History {
  interval_seconds: number;
  /** Upper bounds of the duration buckets, in seconds. */
  buckets: number[];
  /** Oldest first. */
  samples: Sample[];
}

export interface Point {
  time: number;
  value: number;
}

/**
 * Per second increase of a counter, one point per pair of samples. A pair is
 * skipped when the counter is absent from either side or went back (restart).
 */
export function rate(samples: Sample[], pick: (s: Sample) => number | undefined): Point[] {
  const points: Point[] = [];
  for (let i = 1; i < samples.length; i++) {
    const [a, b] = [pick(samples[i - 1]), pick(samples[i])];
    const seconds = samples[i].time - samples[i - 1].time;
    if (a === undefined || b === undefined || b < a || seconds <= 0) continue;
    points.push({ time: samples[i].time, value: (b - a) / seconds });
  }
  return points;
}

/** Bucket deltas and request count of `server` (all when `null`) between `a` and `b`. */
function durations(a: Sample, b: Sample, server: string | null) {
  const names = server === null ? Object.keys(b.servers) : [server];
  const deltas: number[] = [];
  let count = 0;
  for (const name of names) {
    const [before, after] = [a.servers[name], b.servers[name]];
    if (!before || !after) {
      if (server === null) continue; // A server new in `b`: its first requests count from the next pair.
      return null;
    }
    const counted = after.duration_count - before.duration_count;
    if (counted < 0) return null;
    count += counted;
    for (let i = 0; i < after.duration_buckets.length; i++) {
      const delta = after.duration_buckets[i] - (before.duration_buckets[i] ?? 0);
      if (delta < 0) return null;
      deltas[i] = (deltas[i] ?? 0) + delta;
    }
  }
  return { deltas, count };
}

/**
 * Quantile `q` (0 to 1) of the request durations, in seconds, one point per
 * pair of samples, interpolated inside its bucket. `server === null` sums all
 * servers. A quantile beyond the last bound is the last bound.
 */
export function quantile(
  samples: Sample[],
  server: string | null,
  q: number,
  buckets: number[],
): Point[] {
  const points: Point[] = [];
  for (let i = 1; i < samples.length; i++) {
    const pair = durations(samples[i - 1], samples[i], server);
    if (!pair || pair.count === 0) continue;
    const rank = q * pair.count;
    let below = 0;
    let value = buckets[buckets.length - 1];
    for (let bucket = 0; bucket < buckets.length; bucket++) {
      const inside = pair.deltas[bucket] ?? 0;
      if (inside > 0 && below + inside >= rank) {
        const lower = bucket === 0 ? 0 : buckets[bucket - 1];
        value = lower + ((buckets[bucket] - lower) * Math.max(0, rank - below)) / inside;
        break;
      }
      below += inside;
    }
    points.push({ time: samples[i].time, value });
  }
  return points;
}

/** Value of the newest point. */
export function last(points: Point[]): number | null {
  return points.length ? points[points.length - 1].value : null;
}

/** Samples of the last `seconds`, counted back from the newest sample. */
export function window(samples: Sample[], seconds: 300 | 900 | 3600): Sample[] {
  if (!samples.length) return [];
  const from = samples[samples.length - 1].time - seconds;
  return samples.filter((sample) => sample.time >= from);
}
