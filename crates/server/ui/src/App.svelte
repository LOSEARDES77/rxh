<script lang="ts">
  import { onMount } from "svelte";
  import { api, needsToken, setToken } from "./lib/api";
  import Certificates from "./tabs/Certificates.svelte";
  import Hosts from "./tabs/Hosts.svelte";
  import Overview from "./tabs/Overview.svelte";
  import Pools from "./tabs/Pools.svelte";
  import Settings from "./tabs/Settings.svelte";

  type Status = { version: string; uptime_seconds: number; config_path: string | null };

  const tabs = [
    { id: "overview", label: "Overview", component: Overview },
    { id: "hosts", label: "Hosts", component: Hosts },
    { id: "pools", label: "Pools", component: Pools },
    { id: "certificates", label: "Certificates", component: Certificates },
    { id: "settings", label: "Settings", component: Settings },
  ];

  function tabOfHash(): number {
    return Math.max(0, tabs.findIndex((tab) => `#${tab.id}` === location.hash));
  }

  let selected = $state(tabOfHash());
  const Current = $derived(tabs[selected].component);
  let status: Status | undefined = $state();
  let failure = $state("");
  let token = $state("");
  let dialog: HTMLDialogElement;

  async function refresh() {
    try {
      status = await api<Status>("status");
      failure = "";
    } catch (err) {
      failure = err instanceof Error ? err.message : String(err);
    }
  }

  function uptime(seconds: number): string {
    const days = Math.floor(seconds / 86400);
    const hours = Math.floor((seconds % 86400) / 3600);
    const minutes = Math.floor((seconds % 3600) / 60);
    if (days > 0) return `${days}d ${hours}h`;
    if (hours > 0) return `${hours}h ${minutes}m`;
    return `${minutes}m ${seconds % 60}s`;
  }

  function select(index: number) {
    location.hash = tabs[index].id;
  }

  function onTabKey(event: KeyboardEvent) {
    const last = tabs.length - 1;
    const next = {
      ArrowRight: selected === last ? 0 : selected + 1,
      ArrowLeft: selected === 0 ? last : selected - 1,
      Home: 0,
      End: last,
    }[event.key];
    if (next === undefined) return;
    event.preventDefault();
    select(next);
    document.getElementById(`tab-${tabs[next].id}`)?.focus();
  }

  function submitToken(event: SubmitEvent) {
    event.preventDefault();
    setToken(token);
    token = "";
    refresh();
  }

  $effect(() => {
    if ($needsToken && !dialog.open) dialog.showModal();
    if (!$needsToken && dialog.open) dialog.close();
  });

  onMount(() => {
    refresh();
    const timer = setInterval(refresh, 5000);
    return () => clearInterval(timer);
  });
</script>

<svelte:window onhashchange={() => (selected = tabOfHash())} />

<header>
  <h1>RXH</h1>
  {#if status}
    <dl>
      <div><dt>Version</dt><dd>{status.version}</dd></div>
      <div><dt>Uptime</dt><dd>{uptime(status.uptime_seconds)}</dd></div>
      <div><dt>Configuration</dt><dd><code>{status.config_path ?? "none"}</code></dd></div>
    </dl>
  {/if}
  {#if failure}
    <p class="failure" role="alert">{failure}</p>
  {/if}
</header>

<div class="tabs" role="tablist" aria-label="Sections">
  {#each tabs as tab, index (tab.id)}
    <button
      type="button"
      role="tab"
      id="tab-{tab.id}"
      aria-selected={index === selected}
      aria-controls="panel"
      tabindex={index === selected ? 0 : -1}
      onclick={() => select(index)}
      onkeydown={onTabKey}>{tab.label}</button
    >
  {/each}
</div>

<main>
  <div id="panel" role="tabpanel" aria-labelledby="tab-{tabs[selected].id}">
    <Current />
  </div>
</main>

<!-- Without a token every call fails: the dialog stays until one is given. -->
<dialog
  bind:this={dialog}
  aria-labelledby="token-title"
  oncancel={(event) => event.preventDefault()}
  onclose={() => $needsToken && dialog.showModal()}
>
  <form onsubmit={submitToken}>
    <h2 id="token-title">Admin token</h2>
    <p>This admin API asks for the <code>[admin] token</code> of the configuration.</p>
    <!-- svelte-ignore a11y_autofocus -->
    <input type="password" bind:value={token} aria-label="Token" autocomplete="off" required autofocus />
    <button type="submit">Use token</button>
  </form>
</dialog>

<style>
  header {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 0.5rem 2rem;
    padding: 0.75rem 1rem;
    border-bottom: 1px solid var(--border);
    background: var(--surface);
  }

  h1 {
    margin: 0;
    font-size: 1.25rem;
  }

  dl {
    display: flex;
    flex-wrap: wrap;
    gap: 0.25rem 1.5rem;
    margin: 0;
  }

  dl div {
    display: flex;
    gap: 0.4rem;
  }

  dt {
    color: var(--muted);
  }

  dd {
    margin: 0;
    overflow-wrap: anywhere;
  }

  .failure {
    margin: 0;
    color: var(--danger);
  }

  .tabs {
    display: flex;
    gap: 0.25rem;
    padding: 0 1rem;
    border-bottom: 1px solid var(--border);
    overflow-x: auto;
  }

  [role="tab"] {
    padding: 0.6rem 0.9rem;
    border: none;
    border-bottom: 2px solid transparent;
    background: none;
    cursor: pointer;
  }

  [role="tab"][aria-selected="true"] {
    border-bottom-color: var(--accent);
    color: var(--accent);
  }

  [role="tab"]:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }

  main {
    padding: 1rem;
  }

  dialog {
    max-width: min(28rem, calc(100vw - 2rem));
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--background);
    color: var(--text);
  }

  dialog::backdrop {
    background: rgb(0 0 0 / 40%);
  }

  dialog h2 {
    margin-top: 0;
    font-size: 1.1rem;
  }

  dialog form {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }

  dialog p {
    margin: 0;
  }

  dialog input {
    padding: 0.4rem 0.5rem;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--surface);
  }

  dialog button {
    align-self: flex-end;
    padding: 0.4rem 0.9rem;
    border: none;
    border-radius: 4px;
    background: var(--accent);
    color: var(--background);
    cursor: pointer;
  }
</style>
