<script lang="ts">
  import { onMount } from "svelte";
  import { ApiError, api, keep, kept, loadWithToken, needsToken, stopped } from "./lib/api";
  import { refresh as readSession, session, signedIn, signOut } from "./lib/session";
  import SignIn from "./lib/SignIn.svelte";
  import Certificates from "./tabs/Certificates.svelte";
  import Hosts from "./tabs/Hosts.svelte";
  import Overview from "./tabs/Overview.svelte";
  import Pools from "./tabs/Pools.svelte";
  import Settings from "./tabs/Settings.svelte";

  // Without the `admin` role the daemon does not say where its configuration is.
  type Status = { version: string; uptime_seconds: number; config_path?: string | null };

  const SKIPPED_KEY = "rxh-setup-skipped";

  const all = [
    { id: "overview", label: "Overview", component: Overview },
    { id: "hosts", label: "Hosts", component: Hosts },
    { id: "pools", label: "Pools", component: Pools },
    { id: "certificates", label: "Certificates", component: Certificates },
    { id: "settings", label: "Settings", component: Settings },
  ];

  // The other tabs read what only an administrator is answered.
  const tabs = $derived($session?.admin ? all : all.slice(0, 2));
  let hash = $state(location.hash);
  const selected = $derived(Math.max(0, tabs.findIndex((tab) => `#${tab.id}` === hash)));
  const Current = $derived(tabs[selected].component);
  let status: Status | undefined = $state();
  let failure = $state("");
  let changing = $state(false);
  let skipped = $state(kept(() => localStorage, SKIPPED_KEY) !== null);
  let dialog: HTMLDialogElement;

  // The first user can be created by this caller. An administrator may have
  // said "not now"; who needs the code has no other way in.
  const firstRun = $derived(
    $session !== undefined &&
      $session.setup &&
      ($session.admin ? !skipped : $session.setup_needs_code),
  );
  // The form the page asks for, over whatever is open: a 401 comes at any time.
  const form = $derived.by(() => {
    if (firstRun && !$needsToken) return "setup";
    if ($needsToken || ($session !== undefined && !signedIn($session))) return "sign-in";
    return changing ? "password" : undefined;
  });
  // The API answers an administrator without accounts, and a password can stay as it is.
  const leavable = $derived(form === "password" || (form === "setup" && $session?.admin));

  // The sign-in form says what a 401 means: it is no failure to report.
  function report(err: unknown) {
    const expected = err instanceof ApiError && err.status === 401;
    failure = expected ? "" : err instanceof Error ? err.message : String(err);
  }

  async function refresh() {
    // Signed out, nothing is asked until `loadWithToken` says somebody signed in.
    if ($stopped || $needsToken) return;
    try {
      if (!$session) await readSession();
      status = signedIn($session) ? await api<Status>("status") : undefined;
      failure = "";
    } catch (err) {
      report(err);
    }
  }

  async function leave() {
    try {
      await signOut();
    } catch (err) {
      report(err);
    }
    refresh();
  }

  function dismiss() {
    if (!leavable) return;
    if (form === "setup") {
      skipped = true;
      keep(() => localStorage, SKIPPED_KEY, "1");
    }
    changing = false;
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

  // A 401 takes the place of the password form, which does not come back by itself.
  $effect(() => {
    if ($needsToken) changing = false;
  });

  $effect(() => {
    if (form && !dialog.open) dialog.showModal();
    if (!form && dialog.open) dialog.close();
  });

  onMount(() => {
    // Now, and again as soon as somebody signs in.
    const forget = loadWithToken(refresh);
    const timer = setInterval(refresh, 5000);
    return () => {
      forget();
      clearInterval(timer);
    };
  });
</script>

<svelte:window onhashchange={() => (hash = location.hash)} />

<header>
  <h1>RXH</h1>
  {#if status}
    <dl>
      <div><dt>Version</dt><dd>{status.version}</dd></div>
      <div><dt>Uptime</dt><dd>{uptime(status.uptime_seconds)}</dd></div>
      {#if status.config_path !== undefined}
        <div><dt>Configuration</dt><dd><code>{status.config_path ?? "none"}</code></dd></div>
      {/if}
    </dl>
  {/if}
  {#if failure && !$stopped}
    <p class="failure" role="alert">{failure}</p>
  {/if}
  {#if $session?.user && !$needsToken}
    <div class="user">
      <span>{$session.user}</span>
      <button type="button" class="button" onclick={() => (changing = true)}>Change password</button>
      <button type="button" class="button" onclick={leave}>Sign out</button>
    </div>
  {/if}
</header>

<!-- Once stopped, the tabs are taken away so nothing asks the API again. -->
{#if $stopped}
  <main>
    <p class="notice" role="status">
      The daemon was stopped: the proxy no longer serves and this page has no API. Start rxh again
      on its machine, then reload this page.
    </p>
  </main>
{:else if signedIn($session)}
  <!-- Another caller gets nothing of what the one before had on screen, an open form included. -->
  {#key $session?.user}
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
  {/key}
{/if}

<!--
  Nothing works until the caller is somebody: the dialog stays until then,
  over an open form, which keeps what was typed.
-->
<dialog
  bind:this={dialog}
  aria-labelledby="sign-in-title"
  oncancel={(event) => {
    event.preventDefault();
    dismiss();
  }}
  onclose={() => form && dialog.showModal()}
>
  {#if form}
    {#key form}
      <SignIn {form} ondone={() => (changing = false)} oncancel={leavable ? dismiss : undefined} />
    {/key}
  {/if}
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

  .user {
    display: flex;
    flex-wrap: wrap;
    align-items: baseline;
    gap: 0.5rem;
    margin-left: auto;
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
    width: 24rem;
    max-width: calc(100vw - 2rem);
    border: 1px solid var(--border);
    border-radius: 6px;
    background: var(--background);
    color: var(--text);
  }

  dialog::backdrop {
    background: rgb(0 0 0 / 40%);
  }
</style>
