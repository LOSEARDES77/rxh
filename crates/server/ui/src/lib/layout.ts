import { definition, type Kind, type Node, type Schema } from "./schema";

/** Where the keys of a server and of a pool are drawn: the common ones first, the rest by topic. */
export const layout: Record<
  "server" | "upstream",
  { common: string[]; groups: { title: string; keys: string[] }[] }
> = {
  server: {
    common: ["name", "host", "listen", "protocol", "forward", "serve", "redirect", "tls"],
    groups: [
      {
        title: "Access",
        keys: [
          "groups",
          "ip_allow",
          "ip_deny",
          "basic_auth",
          "forward_auth",
          "client_auth",
          "trusted_proxies",
        ],
      },
      { title: "Routes", keys: ["match", "path", "method", "query", "uri", "remote_ip"] },
      {
        title: "Headers and rewrites",
        keys: [
          "header",
          "request",
          "response",
          "rewrite",
          "security_headers",
          "cors",
          "error_pages",
          "handle_response",
        ],
      },
      {
        title: "TLS and protocols",
        keys: [
          "tls_options",
          "http2",
          "http3",
          "https_redirect",
          "proxy_protocol",
          "proxy_protocol_in",
        ],
      },
      { title: "Performance", keys: ["encode", "cache", "rate_limit", "mirror"] },
      {
        title: "Timeouts and limits",
        keys: [
          "idle_timeout",
          "read_header_timeout",
          "match_timeout",
          "stream_timeout",
          "udp_idle_timeout",
          "stream_close_delay",
          "max_connections",
          "max_connections_per_ip",
        ],
      },
    ],
  },
  upstream: {
    common: ["backends", "policy", "key", "retries", "health"],
    groups: [
      { title: "Health", keys: ["passive", "circuit_breaker", "slow_start", "fallback"] },
      {
        title: "Retries",
        keys: ["retry_budget", "retry_match", "try_duration", "try_interval", "hedge_delay"],
      },
      {
        title: "Load balancing",
        keys: ["choose", "hash_type", "hash_balance_factor", "secret", "dynamic"],
      },
      {
        title: "Connections",
        keys: [
          "tls",
          "h2c",
          "grpc_web",
          "keepalive",
          "keepalive_idle_conns_per_host",
          "max_connections",
          "max_conns_per_host",
          "http2_conns_per_host",
          "queue_size",
          "queue_timeout",
        ],
      },
      {
        title: "Timeouts and buffers",
        keys: [
          "dial_timeout",
          "read_timeout",
          "write_timeout",
          "response_header_timeout",
          "flush_interval",
          "request_buffers",
          "response_buffers",
        ],
      },
    ],
  },
};

/**
 * The groups that draw `keys` (those of the schema, and those of the file the
 * schema does not know): a key in no group lands in "Everything else", so
 * none is unreachable. A section has no groups.
 */
export function arrange(
  kind: Kind,
  keys: string[],
): { common: string[]; groups: { title: string; keys: string[] }[] } {
  if (kind === "section") return { common: keys, groups: [] };
  const placed = new Set([
    ...layout[kind].common,
    ...layout[kind].groups.flatMap((group) => group.keys),
  ]);
  const groups = [
    ...layout[kind].groups,
    { title: "Everything else", keys: keys.filter((key) => !placed.has(key)) },
  ];
  return {
    common: layout[kind].common.filter((key) => keys.includes(key)),
    groups: groups
      .map((group) => ({ ...group, keys: group.keys.filter((key) => keys.includes(key)) }))
      .filter((group) => group.keys.length > 0),
  };
}

const ACRONYMS = /^(tls|ip|https?\d?|uri|url|cors|udp|tcp|dns|ca|grpc|h2c|acme|id)$/;

/** The label of a key: `hash_balance_factor` reads "Hash balance factor". */
export function human(key: string): string {
  const words = key.split("_").map((word) => (ACRONYMS.test(word) ? word.toUpperCase() : word));
  return words.join(" ").replace(/^[a-z]/, (letter) => letter.toUpperCase());
}

/** The label of one item of the list labelled `label`, where the singular is not the label. */
const ITEMS: Record<string, string> = {
  Backends: "Backend",
  Listen: "Listen address",
  Host: "Host name",
  Addresses: "Address",
};

export function itemLabel(label: string): string {
  return ITEMS[label] ?? label;
}

/** `label` inside a sentence ("Add health"); an acronym keeps its capitals. */
export function lower(label: string): string {
  return /^[A-Z][a-z]/.test(label) ? label.charAt(0).toLowerCase() + label.slice(1) : label;
}

/** Help of the keys the schema describes only by their shape, or in words of the code. */
const HELP: Record<string, string> = {
  host: "Host names this server answers to, like example.com or *.example.com. Empty: any.",
  listen: "Addresses to listen on, like 0.0.0.0:443. Port 0 picks a free port.",
  forward: "Where requests go: a pool, one address, or several.",
  serve: "Directory whose files are served.",
  redirect: "Address the client is sent to, like https://example.com{uri}.",
  tls: "Off: plain connections. Automatic: certificates from ACME for the host names.",
};

/**
 * The help of `key`: its own description, else the text kept here, else the
 * description of its definition. That of a `OneOrMany_*` definition only
 * describes the shape and is never shown.
 */
export function helpOf(schema: Schema, key: string, node: Node): string | undefined {
  if (node.description) return node.description;
  if (HELP[key]) return HELP[key];
  const name = definition(node);
  return name?.startsWith("OneOrMany_") ? undefined : name && schema.definitions?.[name]?.description;
}
