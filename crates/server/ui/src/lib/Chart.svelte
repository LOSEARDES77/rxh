<script lang="ts">
  import type { Point } from "./stats";

  let { series, unit }: { series: { label: string; points: Point[] }[]; unit: string } = $props();

  const WIDTH = 600;
  const HEIGHT = 160;

  const all = $derived(series.flatMap((line) => line.points));
  const start = $derived(Math.min(...all.map((point) => point.time)));
  const end = $derived(Math.max(...all.map((point) => point.time)));
  const top = $derived(Math.max(...all.map((point) => point.value)) || 1);
  const times = $derived([...new Set(all.map((point) => point.time))].sort((a, b) => a - b));

  function coordinates(points: Point[]): string {
    const span = end - start || 1;
    return points
      .map((p) => `${round(((p.time - start) / span) * WIDTH)},${round(HEIGHT - (p.value / top) * HEIGHT)}`)
      .join(" ");
  }

  function round(n: number): number {
    return Math.round(n * 10) / 10;
  }

  function number(value: number): string {
    return value >= 100 ? value.toFixed(0) : value.toPrecision(2).replace(/\.?0+$/, "");
  }

  function clock(time: number): string {
    return new Date(time * 1000).toLocaleTimeString();
  }
</script>

<figure>
  {#if all.length === 0}
    <p class="empty">No data yet</p>
  {:else}
    <div class="plot">
      <span class="axis">{number(top)} {unit}</span>
      <svg viewBox="0 0 {WIDTH} {HEIGHT}" preserveAspectRatio="none" role="img">
        <title>{series.map((line) => line.label).join(", ")} ({unit})</title>
        <line class="grid" x1="0" y1={HEIGHT} x2={WIDTH} y2={HEIGHT} />
        <line class="grid" x1="0" y1={HEIGHT / 2} x2={WIDTH} y2={HEIGHT / 2} />
        {#each series as line, index (line.label)}
          {#if line.points.length}
            <polyline class="line s{index}" points={coordinates(line.points)} />
          {/if}
        {/each}
      </svg>
      <span class="axis times"><span>{clock(start)}</span><span>{clock(end)}</span></span>
    </div>
  {/if}
  <ul>
    {#each series as line, index (line.label)}
      <li>
        <svg viewBox="0 0 24 8" width="24" height="8" aria-hidden="true">
          <line class="line s{index}" x1="0" y1="4" x2="24" y2="4" />
        </svg>
        {line.label}
      </li>
    {/each}
  </ul>
  {#if all.length}
    <details>
      <summary>Values</summary>
      <table>
        <thead>
          <tr>
            <th scope="col">Time</th>
            {#each series as line (line.label)}<th scope="col">{line.label} ({unit})</th>{/each}
          </tr>
        </thead>
        <tbody>
          {#each times as time (time)}
            <tr>
              <th scope="row">{clock(time)}</th>
              {#each series as line (line.label)}
                {@const point = line.points.find((p) => p.time === time)}
                <td>{point ? number(point.value) : "—"}</td>
              {/each}
            </tr>
          {/each}
        </tbody>
      </table>
    </details>
  {/if}
</figure>

<style>
  figure {
    margin: 0;
    /* Series differ by dash pattern as well as colour. */
    --s0: #2557c7;
    --s1: #1b8a5a;
    --s2: #8a5cc7;
    --s3: #c77a1b;
    --s4: #c73a2f;
  }

  @media (prefers-color-scheme: dark) {
    figure {
      --s0: #7ea3ff;
      --s1: #5fd39b;
      --s2: #c3a0ff;
      --s3: #ffb35c;
      --s4: #ff8a80;
    }
  }

  .plot svg {
    display: block;
    width: 100%;
    height: 10rem;
    border-left: 1px solid var(--border);
  }

  .axis {
    color: var(--muted);
    font-size: 0.8rem;
  }

  .times {
    display: flex;
    justify-content: space-between;
  }

  .grid {
    stroke: var(--border);
    vector-effect: non-scaling-stroke;
  }

  .line {
    fill: none;
    stroke-width: 2;
    vector-effect: non-scaling-stroke;
  }

  .s0 {
    stroke: var(--s0);
  }
  .s1 {
    stroke: var(--s1);
    stroke-dasharray: 8 3;
  }
  .s2 {
    stroke: var(--s2);
    stroke-dasharray: 2 3;
  }
  .s3 {
    stroke: var(--s3);
    stroke-dasharray: 8 3 2 3;
  }
  .s4 {
    stroke: var(--s4);
    stroke-dasharray: 12 4;
  }

  .empty {
    display: grid;
    place-items: center;
    height: 10rem;
    margin: 0;
    border: 1px dashed var(--border);
    color: var(--muted);
  }

  ul {
    display: flex;
    flex-wrap: wrap;
    gap: 0.25rem 1rem;
    margin: 0.5rem 0 0;
    padding: 0;
    list-style: none;
    font-size: 0.85rem;
  }

  li {
    display: flex;
    align-items: center;
    gap: 0.35rem;
  }

  details {
    margin-top: 0.25rem;
    font-size: 0.85rem;
  }

  table {
    border-collapse: collapse;
  }

  th,
  td {
    padding: 0.1rem 0.6rem 0.1rem 0;
    text-align: right;
    font-variant-numeric: tabular-nums;
  }
</style>
