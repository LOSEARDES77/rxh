import { fireEvent, render, screen } from "@testing-library/svelte";
import { SvelteMap } from "svelte/reactivity";
import rxhSchema from "../../../../../../docs/rxh.schema.json";
import SchemaField from "../SchemaField.svelte";
import type { Node, Schema } from "../schema";

const schema = rxhSchema as unknown as Schema;
const server = schema.definitions?.Server.properties as Record<string, Node>;

/** Test helper: draws the key `key` of a server as the editor does, bound to a value the test reads. */
export function draw(key: string, initial: unknown, pools: string[] = []) {
  const box = new SvelteMap<string, unknown>([["value", initial]]);
  render(SchemaField, {
    context: new Map([["pools", () => pools]]),
    props: {
      schema,
      node: server[key],
      label: key,
      path: key,
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

export function type(label: string, text: string) {
  return fireEvent.input(screen.getByLabelText(label), { target: { value: text } });
}

export function click(role: "radio" | "button", name: string) {
  return fireEvent.click(screen.getByRole(role, { name }));
}

export function checked(): string | null | undefined {
  const radios: HTMLInputElement[] = screen.getAllByRole("radio");
  return radios.find((radio) => radio.checked)?.parentElement?.textContent?.trim();
}
