import { fireEvent, render, screen } from "@testing-library/svelte";
import { SvelteMap } from "svelte/reactivity";
import rxhSchema from "../../../../../../docs/rxh.schema.json";
import { helpOf, human } from "../layout";
import SchemaField from "../SchemaField.svelte";
import type { Node, Schema } from "../schema";

const schema = rxhSchema as unknown as Schema;
const server = schema.definitions?.Server.properties as Record<string, Node>;
const pool = schema.definitions?.UpstreamGroup.properties as Record<string, Node>;

/** Test helper: draws the key `key` of a server (or of a pool) as the editor does, bound to a value the test reads. */
export function draw(key: string, initial: unknown, pools: string[] = [], required = false) {
  const box = new SvelteMap<string, unknown>([["value", initial]]);
  render(SchemaField, {
    context: new Map([["pools", () => pools]]),
    props: {
      schema,
      node: server[key] ?? pool[key],
      label: human(key),
      name: key,
      help: helpOf(schema, key, server[key] ?? pool[key]),
      path: key,
      required,
      get value() {
        return box.get("value");
      },
      set value(next: unknown) {
        box.set("value", next);
      },
    },
  });
  return () => box.get("value");
}

/** The box labelled `label`; a radio may carry the same word. */
export function field<T extends HTMLElement = HTMLInputElement>(label: string): T {
  return screen.getByLabelText(label, { selector: "input:not([type=radio]), select, textarea" });
}

export function type(label: string, text: string) {
  return fireEvent.input(field(label), { target: { value: text } });
}

export function click(role: "radio" | "button", name: string) {
  return fireEvent.click(screen.getByRole(role, { name }));
}

export function checked(): string | null | undefined {
  const radios: HTMLInputElement[] = screen.getAllByRole("radio");
  return radios.find((radio) => radio.checked)?.parentElement?.textContent?.trim();
}
