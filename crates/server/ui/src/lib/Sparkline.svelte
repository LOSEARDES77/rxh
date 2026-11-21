<script lang="ts">
  import type { Point } from "./stats";

  let { points }: { points: Point[] } = $props();

  const WIDTH = 80;
  const HEIGHT = 20;

  const line = $derived.by(() => {
    const start = points[0]?.time ?? 0;
    const span = (points[points.length - 1]?.time ?? 0) - start || 1;
    const top = Math.max(...points.map((p) => p.value)) || 1;
    return points
      .map((p) => {
        const x = Math.round(((p.time - start) / span) * WIDTH * 10) / 10;
        const y = Math.round((HEIGHT - (p.value / top) * HEIGHT) * 10) / 10;
        return `${x},${y}`;
      })
      .join(" ");
  });
</script>

{#if points.length < 2}
  —
{:else}
  <svg
    class="sparkline"
    viewBox="0 0 {WIDTH} {HEIGHT}"
    width={WIDTH}
    height={HEIGHT}
    preserveAspectRatio="none"
    role="img"
    aria-label="Requests per second trend, last value {points[points.length - 1].value.toFixed(1)}"
  >
    <polyline points={line} />
  </svg>
{/if}

<style>
  .sparkline {
    vertical-align: middle;
    overflow: visible;
  }

  polyline {
    fill: none;
    stroke: var(--accent);
    stroke-width: 1.5;
    vector-effect: non-scaling-stroke;
  }
</style>
