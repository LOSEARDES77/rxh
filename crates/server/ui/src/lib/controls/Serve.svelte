<script lang="ts">
  import Options from "./Options.svelte";
  import type { Node, Schema } from "../schema";

  let {
    value = $bindable(),
    ...rest
  }: { schema: Schema; node: Node; value: unknown; label: string; path: string } = $props();
</script>

<Options {...rest} main="root" text="Directory" bind:value />
