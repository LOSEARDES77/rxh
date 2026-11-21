import { fireEvent, render, screen } from "@testing-library/svelte";
import { afterEach, expect, test, vi } from "vitest";
import Certificates from "./Certificates.svelte";
import { serve, writes } from "./serve";

function certificate(names: string[], days_left: number, more: object = {}) {
  return {
    listen: "127.0.0.1:8443",
    subject: `CN=${names[0]}`,
    issuer: "CN=Test CA",
    names,
    not_before: "2026-01-01T00:00:00Z",
    not_after: `in ${days_left} days`,
    days_left,
    hosts: names,
    default: false,
    source: "file",
    cert_file: "/etc/rxh/cert.pem",
    key_file: "/etc/rxh/key.pem",
    ocsp_stapled: false,
    ...more,
  };
}

// Out of order on purpose: the page sorts.
const CERTIFICATES = [
  certificate(["late.example"], 200, { source: "acme", issuer: "CN=R11", ocsp_stapled: true }),
  certificate(["soon.example", "www.soon.example"], 10),
  certificate(["gone.example"], -2),
  certificate(["edge.example"], 30),
];

const FILES = {
  editable: true,
  files: [{ index: 0, path: "/etc/rxh/rxh.toml", revision: "r1", text: "" }],
  entities: [],
};

afterEach(() => vi.unstubAllGlobals());

function rows(): string[] {
  return screen
    .getAllByRole("row")
    .slice(1)
    .map((row) => [...row.children].map((cell) => cell.textContent?.replace(/\s+/g, " ").trim()).join(" | "));
}

test("certificates_are_sorted_by_expiry_and_flagged_under_30_days", async () => {
  serve({ certificates: CERTIFICATES, "config/files": FILES });
  render(Certificates);
  await screen.findByText("late.example");
  const shown = rows();
  expect(shown.map((row) => row.split(" | ")[0])).toEqual([
    "gone.example",
    "soon.example, www.soon.example",
    "edge.example",
    "late.example",
  ]);
  expect(shown[0]).toContain("| -2 expired |");
  expect(shown[1]).toContain("| 10 expires soon |");
  expect(shown[1]).toContain("127.0.0.1:8443");
  expect(shown[2]).not.toContain("expires soon");
  expect(shown[3]).toMatch(/acme \| CN=R11 \| .* \| 200 \| stapled$/);
  expect(shown[1]).toContain("not stapled");
});

test("reload_certificates_reports_what_was_read", async () => {
  const fetch = serve(
    { certificates: CERTIFICATES, "config/files": FILES },
    { status: 200, body: { reloaded: ["/etc/rxh/cert.pem"] } },
  );
  render(Certificates);
  await fireEvent.click(await screen.findByRole("button", { name: "Reload certificates" }));
  expect(writes(fetch)).toEqual([{ url: "certificates/reload", method: "POST", body: undefined }]);
  expect((await screen.findByRole("status")).textContent).toContain("1 certificate file read again");
});

test("a_failed_certificate_reload_is_reported", async () => {
  serve(
    { certificates: CERTIFICATES, "config/files": FILES },
    { status: 400, body: { error: "/etc/rxh/cert.pem: no such file" } },
  );
  render(Certificates);
  await fireEvent.click(await screen.findByRole("button", { name: "Reload certificates" }));
  expect((await screen.findByRole("alert")).textContent).toContain("/etc/rxh/cert.pem: no such file");
});

test("the_acme_section_is_created_with_post_when_absent", async () => {
  const fetch = serve({ certificates: [], "config/files": FILES }, { status: 201, body: { key: "acme" } });
  render(Certificates);
  expect(await screen.findByText("No TLS certificate is served.")).toBeTruthy();
  expect(screen.getByText("not set")).toBeTruthy();
  await fireEvent.click(screen.getByRole("button", { name: "Set up ACME" }));
  await fireEvent.click(screen.getByRole("button", { name: "Save and reload" }));
  await vi.waitFor(() => expect(writes(fetch)).toHaveLength(1));
  expect(writes(fetch)[0]).toEqual({
    url: "config/entities/section",
    method: "POST",
    body: { value: {}, key: "acme", file: 0 },
  });
});

test("the_acme_section_of_the_file_is_edited_in_place", async () => {
  const acme = { kind: "section", key: "acme", file: 0, value: { email: "a@example.com" }, toml: "" };
  serve({ certificates: [], "config/files": { ...FILES, entities: [acme] } });
  render(Certificates);
  await fireEvent.click(await screen.findByRole("button", { name: "Edit ACME" }));
  expect(screen.getByRole("heading", { name: "Section acme" })).toBeTruthy();
});

test("without_a_configuration_file_acme_is_read_only", async () => {
  serve({ certificates: CERTIFICATES, "config/files": { editable: false, files: [], entities: [] } });
  render(Certificates);
  expect(await screen.findByText("No configuration file: read only")).toBeTruthy();
  expect(screen.queryByRole("button", { name: /ACME/ })).toBeNull();
  expect(screen.getByRole("button", { name: "Reload certificates" })).toBeTruthy();
});
