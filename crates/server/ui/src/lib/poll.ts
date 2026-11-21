/**
 * Calls `refresh` now and every five seconds while the page is visible, and
 * at once when it shows again. Returns what stops it, for `onMount`.
 */
export function poll(refresh: () => void): () => void {
  let timer: ReturnType<typeof setInterval> | undefined;
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
}
