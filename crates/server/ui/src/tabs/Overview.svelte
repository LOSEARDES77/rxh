<script lang="ts">
  import { onMount } from "svelte";
  import { api } from "../lib/api";
  import Chart from "../lib/Chart.svelte";
  import { last, quantile, rate, window, type History, type Point, type Sample } from "../lib/stats";

  type Certificate = { listen: string; subject: string; names: string[]; days_left: number };
  type Range = 300 | 900 | 3600;

  const RANGES: { seconds: Range; label: string }[] = [
    { seconds: 300, label: "5m" },
    { seconds: 900, label: "15m" },
    { seconds: 3600, label: "1h" },
  ];
  const CLASSES = ["1xx", "2xx", "3xx", "4xx", "5xx"];

  let history: History | undefined = $state();
  let certificates: Certificate[] = $state([]);
  let failure = $state("");
  let range: Range = $state(3600);

  const samples = $derived(history ? window(history.samples, range) : []);
  const newest: Sample | undefined = $derived(samples[samples.length - 1]);
  const buckets = $derived(history?.buckets ?? []);

  function sum(values: (number | undefined)[]): number {
    return values.reduce<number>((total, value) => total + (value ?? 0), 0);
  }

  function total(points: Point[]): number {
    return sum(points.map((point) => point.value));
  }

  /** Responses of every server with a status of class `index` (0 is 1xx). */
  function status(sample: Sample, index: number): number {
    return sum(Object.values(sample.servers).map((server) => server.status[index]));
  }

  function responses(sample: Sample): number {
    return sum(CLASSES.map((_, index) => status(sample, index)));
  }

  const byClass = $derived(
    CLASSES.map((label, index) => ({ label, points: rate(samples, (s) => status(s, index)) })),
  );
  const latency = $derived(
    [0.5, 0.95, 0.99].map((q) => ({
      label: `p${q * 100}`,
      points: quantile(samples, null, q, buckets).map((p) => ({ time: p.time, value: p.value * 1000 })),
    })),
  );

  const requestsPerSecond = $derived(last(rate(samples, responses)));
  const errorShare = $derived.by(() => {
    const errors = last(byClass[4].points);
    return requestsPerSecond && errors !== null ? errors / requestsPerSecond : null;
  });
  const p95 = $derived(last(latency[1].points));
  const pools = $derived(Object.values(newest?.pools ?? {}));
  const inFlight = $derived(pools.length ? sum(pools.map((pool) => pool.active)) : null);
  const healthy = $derived(
    pools.length ? `${sum(pools.map((p) => p.healthy))}/${sum(pools.map((p) => p.backends))}` : "—",
  );

  /** One row of the table, averaged over the range. */
  type Row = {
    name: string;
    perSecond: number | null;
    errors: number | null;
    latency: number | null;
    inFlight?: number;
    healthy?: string;
  };

  function mean(points: Point[]): number | null {
    return points.length ? total(points) / points.length : null;
  }

  /** Share of `part` in `whole`, both per second rates over the same pairs. */
  function share(part: Point[], whole: Point[]): number | null {
    return total(whole) ? total(part) / total(whole) : null;
  }

  const poolRows: Row[] = $derived(
    Object.entries(newest?.pools ?? {}).map(([name, pool]) => {
      const requests = rate(samples, (s) => s.pools[name]?.requests);
      const errors = rate(samples, (s) => {
        const pool = s.pools[name];
        return pool && pool.failures + pool.server_errors;
      });
      const seconds = rate(samples, (s) => s.pools[name]?.response_seconds);
      const responses = rate(samples, (s) => s.pools[name]?.responses);
      const latency = share(seconds, responses);
      return {
        name,
        perSecond: mean(requests),
        errors: share(errors, requests),
        latency: latency === null ? null : latency * 1000,
        inFlight: pool.active,
        healthy: `${pool.healthy}/${pool.backends}`,
      };
    }),
  );

  const serverRows: Row[] = $derived(
    Object.keys(newest?.servers ?? {}).map((name) => {
      // TCP and UDP servers count connections; HTTP ones count responses.
      const served = rate(samples, (s) => {
        const server = s.servers[name];
        return server && sum(server.status) + server.connections;
      });
      const errors = rate(samples, (s) => s.servers[name]?.status[4]);
      const p95 = last(quantile(samples, name, 0.95, buckets));
      return {
        name,
        perSecond: mean(served),
        errors: share(errors, served),
        latency: p95 === null ? null : p95 * 1000,
      };
    }),
  );

  const expiring = $derived(certificates.filter((certificate) => certificate.days_left <= 30));

  function fixed(value: number | null | undefined, digits: number, suffix = ""): string {
    return value === null || value === undefined ? "—" : `${value.toFixed(digits)}${suffix}`;
  }

  function percent(value: number | null): string {
    return value === null ? "—" : `${(value * 100).toFixed(1)}%`;
  }

  async function refresh() {
    try {
      [history, certificates] = await Promise.all([
        api<History>("stats/history"),
        api<Certificate[]>("certificates"),
      ]);
      failure = "";
    } catch (err) {
      failure = err instanceof Error ? err.message : String(err);
    }
  }

  onMount(() => {
    let timer: ReturnType<typeof setInterval> | undefined;
    // No requests while the tab is hidden; catch up at once when it shows again.
    function follow() {
      clearInterval(timer);
      timer = undefined;
      if (document.visibilityState === "hidden") return;
      refresh();
      timer = setInterval(refresh, 5000);
    }
    follow();
    document.addEventListener("visibilitychange", follow);
    return () => {
      clearInterval(timer);
      document.removeEventListener("visibilitychange", follow);
    };
  });
</script>

{#if failure}
  <p class="failure" role="alert">{failure}</p>
{/if}

<dl class="totals">
  <div><dt>Requests per second</dt><dd>{fixed(requestsPerSecond, 1)}</dd></div>
  <div><dt>Errors (5xx)</dt><dd>{percent(errorShare)}</dd></div>
  <div><dt>Latency p95</dt><dd>{fixed(p95, 1, " ms")}</dd></div>
  <div><dt>In flight</dt><dd>{fixed(inFlight, 0)}</dd></div>
  <div><dt>Healthy backends</dt><dd>{healthy}</dd></div>
</dl>

<div class="range" role="group" aria-label="Range">
  {#each RANGES as choice (choice.seconds)}
    <button type="button" aria-pressed={range === choice.seconds} onclick={() => (range = choice.seconds)}
      >{choice.label}</button
    >
  {/each}
</div>

<div class="charts">
  <section>
    <h2>Requests per second, by status class</h2>
    <Chart series={byClass} unit="req/s" />
  </section>
  <section>
    <h2>Latency p50, p95, p99</h2>
    <Chart series={latency} unit="ms" />
  </section>
</div>

<div class="scroll">
<table>
  <caption>Averages over the range; latency is the mean for pools and p95 for servers.</caption>
  <thead>
    <tr>
      <th scope="col">Name</th>
      <th scope="col">Requests/s</th>
      <th scope="col">Errors</th>
      <th scope="col">Latency</th>
      <th scope="col">In flight</th>
      <th scope="col">Healthy</th>
    </tr>
  </thead>
  <tbody>
    {#each poolRows as row (row.name)}
      <tr>
        <th scope="row">Pool {row.name}</th>
        <td>{fixed(row.perSecond, 1)}</td>
        <td>{percent(row.errors)}</td>
        <td>{fixed(row.latency, 1, " ms")}</td>
        <td>{row.inFlight}</td>
        <td>{row.healthy}</td>
      </tr>
    {/each}
    {#each serverRows as row (row.name)}
      <tr>
        <th scope="row">Server {row.name}</th>
        <td>{fixed(row.perSecond, 1)}</td>
        <td>{percent(row.errors)}</td>
        <td>{fixed(row.latency, 1, " ms")}</td>
        <td>—</td>
        <td>—</td>
      </tr>
    {:else}
      {#if !poolRows.length}
        <tr><td colspan="6">No pool or server has counters yet.</td></tr>
      {/if}
    {/each}
  </tbody>
</table>
</div>

{#if expiring.length}
  <section class="certificates">
    <h2>Certificates expiring within 30 days</h2>
    <ul>
      {#each expiring as certificate, index (index)}
        <li>
          {certificate.names[0] ?? certificate.subject} on {certificate.listen}:
          {certificate.days_left < 0 ? "expired" : `${certificate.days_left} d left`}
        </li>
      {/each}
    </ul>
  </section>
{/if}

<style>
  .failure {
    color: var(--danger);
  }

  .totals {
    display: flex;
    flex-wrap: wrap;
    gap: 0.75rem 2.5rem;
    margin: 0 0 1rem;
  }

  .totals dt {
    color: var(--muted);
    font-size: 0.85rem;
  }

  .totals dd {
    margin: 0;
    font-size: 1.5rem;
    font-variant-numeric: tabular-nums;
  }

  .range {
    display: flex;
    gap: 0.25rem;
    margin-bottom: 0.75rem;
  }

  .range button {
    padding: 0.2rem 0.7rem;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--surface);
    cursor: pointer;
  }

  .range button[aria-pressed="true"] {
    border-color: var(--accent);
    color: var(--accent);
    font-weight: 600;
  }

  .charts {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(min(100%, 26rem), 1fr));
    gap: 1.5rem;
    margin-bottom: 1.5rem;
  }

  h2 {
    margin: 0 0 0.5rem;
    font-size: 1rem;
  }

  table {
    border-collapse: collapse;
    width: 100%;
    font-variant-numeric: tabular-nums;
  }

  caption {
    caption-side: bottom;
    padding-top: 0.4rem;
    color: var(--muted);
    font-size: 0.85rem;
    text-align: left;
  }

  th,
  td {
    padding: 0.35rem 0.75rem 0.35rem 0;
    border-bottom: 1px solid var(--border);
    text-align: right;
  }

  th:first-child {
    text-align: left;
  }

  thead th {
    color: var(--muted);
    font-weight: normal;
  }

  .scroll {
    overflow-x: auto;
  }

  .certificates {
    margin-top: 1.5rem;
  }
</style>
