import type { Component } from "svelte";
import { definition, type Node, type Schema } from "../schema";
import Backends from "./Backends.svelte";
import Forward from "./Forward.svelte";
import Redirect from "./Redirect.svelte";
import Serve from "./Serve.svelte";
import Tls from "./Tls.svelte";

export type Custom = Component<
  { schema: Schema; node: Node; value: unknown; label: string; path: string; required?: boolean },
  Record<string, never>,
  "value"
>;

/** The keys drawn by a control of their own, with the definition it is made for. */
const CONTROLS: Record<string, [string, Custom]> = {
  forward: ["Forward", Forward],
  tls: ["Tls", Tls],
  serve: ["FileServer", Serve],
  redirect: ["Redirect", Redirect],
  backends: ["Backend", Backends],
};

/**
 * The hand made control of the key `path` ends in. The definition of the key
 * (or of its items) is checked too: `tls` of a pool is another table.
 */
export function custom(path: string, node: Node): Custom | undefined {
  const [name, component] = CONTROLS[path.split(".").pop() ?? ""] ?? [];
  const matches = name === definition(node) || name === definition(node.items ?? {});
  return name !== undefined && matches ? component : undefined;
}
