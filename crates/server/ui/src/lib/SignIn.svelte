<script lang="ts">
  import { untrack } from "svelte";
  import { get } from "svelte/store";
  import { setToken } from "./api";
  import { changePassword, refresh, session, setup, signedIn, signIn } from "./session";

  let {
    // The ways in, and the password of who is in.
    form,
    // After a password change: the other forms go away with the session they change.
    ondone = undefined,
    // Given when the form may be left without an answer.
    oncancel = undefined,
  }: {
    form: "sign-in" | "setup" | "password";
    ondone?: () => void;
    oncancel?: () => void;
  } = $props();

  const TITLES = {
    "sign-in": "Sign in",
    https: "Sign in",
    token: "Admin token",
    setup: "Create the first user",
    password: "Change password",
  };

  // Where no password is taken, or no user exists to have one, the token is the only way in.
  const byPassword = $derived($session !== undefined && $session.password_login && !$session.setup);
  // No session is given on plain HTTP off this machine: the same page over HTTPS, where the
  // listener has it, can have one.
  const overHttp = $derived(
    $session !== undefined && !$session.password_login && $session.tls && location.protocol === "http:",
  );
  const httpsUrl = $derived(overHttp ? location.href.replace(/^http:/, "https:") : "");
  let byToken = $state(untrack(() => !byPassword && !overHttp));
  const view = $derived.by(() => {
    if (form === "sign-in" && byToken) return "token";
    if (form === "sign-in" && overHttp) return "https";
    return form;
  });
  // The provider's sign-in needs a session cookie, which this connection takes. The sign-in
  // form comes with a 401, while the session read before may still name a user.
  const byProvider = $derived(
    $session?.oidc === true &&
      $session.password_login &&
      (form === "sign-in" || (form === "setup" && !signedIn($session))),
  );

  // Kept here only: they go away with the form.
  let name = $state("");
  let password = $state("");
  let repeat = $state("");
  let current = $state("");
  let code = $state("");
  let token = $state("");
  let failure = $state("");
  let busy = $state(false);

  async function send() {
    if (view === "token") {
      setToken(token);
      await refresh();
      if (!signedIn(get(session))) throw new Error("The daemon did not accept this token.");
    } else if (view === "sign-in") {
      await signIn(name, password);
    } else if (view === "setup") {
      await setup(name, password, code);
    } else {
      await changePassword(current, password);
    }
  }

  async function submit(event: SubmitEvent) {
    event.preventDefault();
    failure = "";
    if (view !== "sign-in" && view !== "token" && password !== repeat) {
      failure = "The two passwords differ.";
      return;
    }
    busy = true;
    try {
      await send();
      ondone?.();
    } catch (err) {
      failure = err instanceof Error ? err.message : String(err);
    } finally {
      busy = false;
      password = repeat = current = token = "";
    }
  }

  function swap() {
    byToken = !byToken;
    failure = "";
  }
</script>

<form onsubmit={submit}>
  <h2 id="sign-in-title">{TITLES[view]}</h2>

  {#if byProvider}
    <a class="primary" href="auth/oidc/login">Sign in with {$session?.oidc_issuer}</a>
  {/if}

  {#if view === "https"}
    <p>Open this page over https to sign in.</p>
    <p><a href={httpsUrl}>{httpsUrl}</a></p>
  {:else if view === "token"}
    <p>This admin API asks for the <code>[admin] token</code> of the configuration.</p>
    <label>
      Token
      <!-- svelte-ignore a11y_autofocus -->
      <input type="password" bind:value={token} autocomplete="off" required autofocus />
    </label>
  {:else}
    {#if view === "setup"}
      <p>No user exists yet. The first one is an administrator.</p>
    {/if}
    {#if view === "password"}
      <!-- For password managers: whose password this is. -->
      <input
        class="visually-hidden"
        type="text"
        autocomplete="username"
        value={$session?.user ?? ""}
        tabindex="-1"
        aria-hidden="true"
        readonly
      />
      <label>
        Current password
        <!-- svelte-ignore a11y_autofocus -->
        <input type="password" bind:value={current} autocomplete="current-password" required autofocus />
      </label>
    {:else}
      <label>
        Name
        <!-- svelte-ignore a11y_autofocus -->
        <input type="text" bind:value={name} autocomplete="username" autocapitalize="none" required autofocus />
      </label>
    {/if}
    {#if view === "sign-in"}
      <label>
        Password
        <input type="password" bind:value={password} autocomplete="current-password" required />
      </label>
    {:else}
      <label>
        {view === "password" ? "New password" : "Password"}
        <input
          type="password"
          bind:value={password}
          autocomplete="new-password"
          minlength="8"
          aria-describedby="sign-in-rule"
          required
        />
      </label>
      <label>
        Repeat the password
        <input type="password" bind:value={repeat} autocomplete="new-password" required />
      </label>
      <p id="sign-in-rule" class="hint">At least 8 characters.</p>
    {/if}
    {#if view === "setup" && $session?.setup_needs_code}
      <label>
        Setup code
        <input type="text" bind:value={code} autocomplete="off" autocapitalize="none" spellcheck="false" required />
      </label>
      <p class="hint">The daemon writes it to its log when it starts without a user.</p>
    {/if}
  {/if}

  {#if failure}
    <p class="failure" role="alert">{failure}</p>
  {/if}

  <div class="actions">
    {#if form === "sign-in" && (!byToken || byPassword || overHttp)}
      <button type="button" class="link" onclick={swap}>
        {byToken ? (overHttp ? "Back" : "Sign in with a password") : "Use the admin token"}
      </button>
    {/if}
    {#if oncancel}
      <button type="button" onclick={oncancel}>{form === "setup" ? "Not now" : "Cancel"}</button>
    {/if}
    {#if view !== "https"}
      <button type="submit" class="primary" disabled={busy}>
        {{ "sign-in": "Sign in", token: "Use token", setup: "Create user", password: "Save password" }[view]}
      </button>
    {/if}
  </div>
</form>

<style>
  form {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }

  h2 {
    margin: 0;
    font-size: 1.1rem;
  }

  p {
    margin: 0;
  }

  label {
    display: grid;
    gap: 0.2rem;
  }

  input {
    width: 100%;
    padding: 0.4rem 0.5rem;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--surface);
  }

  .hint {
    color: var(--muted);
    font-size: 0.85rem;
  }

  .failure {
    margin: 0;
  }

  .actions {
    display: flex;
    flex-wrap: wrap;
    justify-content: flex-end;
    align-items: center;
    gap: 0.5rem;
  }

  button {
    padding: 0.4rem 0.9rem;
    border: 1px solid var(--border);
    border-radius: 4px;
    background: var(--surface);
    cursor: pointer;
  }

  .primary {
    border-color: var(--accent);
    background: var(--accent);
    color: var(--background);
  }

  a.primary {
    padding: 0.4rem 0.9rem;
    border: 1px solid var(--accent);
    border-radius: 4px;
    text-align: center;
    text-decoration: none;
  }

  p a {
    overflow-wrap: anywhere;
  }

  .link {
    margin-right: auto;
    padding-left: 0;
    border: none;
    background: none;
    color: var(--accent);
    text-decoration: underline;
  }
</style>
