// Ticket detail actions (`TicketActions`, `TicketView`'s `ActionPanel`), for every state that
// offers one: draft (Queue it), escalated (Retry / Retry with guidance), submitted (Accept /
// Reject), Cancel on any non-terminal ticket, and no actions at all on a terminal one. Each test
// drives the action from the ticket detail page (`ticket/${id}`) and then checks the *server's*
// state over `api`, not just what the UI shows next — the UI reconciles from the same SSE stream
// a real client would, so a UI-only assertion could pass on a client that drew the right thing for
// the wrong reason.
//
// Buttons carry `data-testid="action-<choice>"` (`TicketActions.tsx`) only while the initial
// choice picker is shown; picking a choice that needs text (`reject`, `retry_with_guidance`)
// swaps in a prompt form instead, found by its label (`getByLabel`) and its submit button's own
// accessible name (`PROMPT[mode].send`, distinct from the picker button's own label, which the
// component appends "…" to for exactly these two choices).
import { expect, test } from "./fixtures";

test("draft: Queue it activates the ticket", async ({ page, api }) => {
  const id = await api.createTicket("Detail actions: queue this draft");
  expect((await api.getTicket(id)).state).toBe("draft");

  await page.goto(`ticket/${id}`);
  await expect(page.getByTestId("action-panel")).toBeVisible();
  await page.getByTestId("action-queue").click();

  // Draft -> ready, then the in-process workers lease it within ~2s. Accept whichever of those
  // states we land on; the point is it left draft.
  const ticket = await api.waitForState(id, ["ready", "leased", "running", "escalated"], {
    timeoutMs: 15_000,
  });
  expect(ticket.state).not.toBe("draft");
});

test("escalated: Retry (no guidance) gives it a fresh round of attempts, objective unchanged", async ({
  page,
  api,
}) => {
  const id = await api.seedEscalated("Detail actions: retry without guidance");
  const before = await api.getTicket(id);
  expect(before.attempts).toBe(3);
  const maxBefore = (before.retry as { max_attempts?: number } | null)?.max_attempts ?? 0;

  await page.goto(`ticket/${id}`);
  await page.getByTestId("action-retry").click();

  await expect
    .poll(async () => (await api.getTicket(id)).state, { timeout: 15_000 })
    .not.toBe("escalated");

  const after = await api.getTicket(id);
  const maxAfter = (after.retry as { max_attempts?: number } | null)?.max_attempts ?? 0;
  expect(maxAfter).toBeGreaterThan(maxBefore);
  expect(after.objective).toBe(before.objective);
});

test("escalated: Retry with guidance requires text, then appends it to the objective", async ({
  page,
  api,
}) => {
  const id = await api.seedEscalated("Detail actions: retry with guidance");
  const before = await api.getTicket(id);
  const maxBefore = (before.retry as { max_attempts?: number } | null)?.max_attempts ?? 0;

  await page.goto(`ticket/${id}`);
  await page.getByTestId("action-retry_with_guidance").click();

  const field = page.getByLabel("Guidance for the next attempt");
  const send = page.getByRole("button", { name: "Retry with guidance", exact: true });
  await expect(send).toBeDisabled();

  const guidance = "Use the existing retry helper instead of a new loop.";
  await field.fill(guidance);
  await expect(send).toBeEnabled();
  await send.click();

  await expect
    .poll(async () => (await api.getTicket(id)).state, { timeout: 15_000 })
    .not.toBe("escalated");

  const after = await api.getTicket(id);
  expect(after.objective).toBe(`${before.objective}\n\nFrom the user, after attempt ${before.attempts}: ${guidance}`);
  const maxAfter = (after.retry as { max_attempts?: number } | null)?.max_attempts ?? 0;
  expect(maxAfter).toBeGreaterThan(maxBefore);
});

test("submitted: Accept closes the ticket", async ({ page, api }) => {
  const { id } = await api.seedSubmitted("Detail actions: accept this submission");

  await page.goto(`ticket/${id}`);
  await page.getByTestId("action-accept").click();

  const ticket = await api.waitForState(id, "closed", { timeoutMs: 15_000 });
  expect(ticket.state).toBe("closed");
});

test("submitted: Reject requires a reason, rejected client-side, then sends the ticket back", async ({
  page,
  api,
}) => {
  const { id } = await api.seedSubmitted("Detail actions: reject this submission");

  await page.goto(`ticket/${id}`);
  await page.getByTestId("action-reject").click();

  const field = page.getByLabel("What should the next attempt fix?");
  const send = page.getByRole("button", { name: "Send back", exact: true });
  await expect(send).toBeDisabled();

  // An empty reason never reaches the server: the button stays disabled and the ticket stays
  // submitted while the field is empty.
  expect((await api.getTicket(id)).state).toBe("submitted");

  const reason = "The tests don't cover the empty-input case.";
  await field.fill(reason);
  await expect(send).toBeEnabled();
  await send.click();

  // Right after reject it already reads `ready` (it goes through `rework`); the workers then
  // pick it up and the mock fails it again. Either way it must leave `submitted`.
  await expect
    .poll(async () => (await api.getTicket(id)).state, { timeout: 10_000 })
    .not.toBe("submitted");

  const after = await api.getTicket(id);
  const failures = after.failures as Array<{ detail: string; class: string }>;
  expect(failures.length).toBeGreaterThan(0);
  expect(failures[failures.length - 1].detail).toBe(reason);
});

test("cancel: asks to confirm, and \"Keep it\" backs out without cancelling", async ({ page, api }) => {
  const id = await api.createTicket("Detail actions: cancel, then keep it");

  await page.goto(`ticket/${id}`);
  await page.getByTestId("action-cancel").click();

  const confirm = page.getByRole("group", { name: "Confirm cancel" });
  await expect(confirm).toBeVisible();
  await expect(confirm).toContainText(`Cancel ${id}?`);

  await confirm.getByRole("button", { name: "Keep it" }).click();
  await expect(confirm).not.toBeVisible();
  await expect(page.getByTestId("action-cancel")).toBeVisible();

  expect((await api.getTicket(id)).state).toBe("draft");
});

test("cancel: confirming cancels a non-terminal ticket", async ({ page, api }) => {
  const id = await api.createTicket("Detail actions: cancel and confirm it");

  await page.goto(`ticket/${id}`);
  await page.getByTestId("action-cancel").click();

  const confirm = page.getByRole("group", { name: "Confirm cancel" });
  await confirm.getByRole("button", { name: `Cancel ${id}`, exact: true }).click();

  const ticket = await api.waitForState(id, "cancelled", { timeoutMs: 10_000 });
  expect(ticket.state).toBe("cancelled");
});

test("terminal tickets (closed, cancelled) show no actions", async ({ page, api }) => {
  const cancelledId = await api.createTicket("Detail actions: terminal, cancelled shows nothing");
  await api.cancel(cancelledId, "e2e: terminal-actions check");
  await api.waitForState(cancelledId, "cancelled");

  await page.goto(`ticket/${cancelledId}`);
  await expect(page.getByTestId("ticket-view")).toBeVisible();
  await expect(page.locator('[data-testid^="action-"]')).toHaveCount(0);

  const { id: submittedId } = await api.seedSubmitted("Detail actions: terminal, closed shows nothing");
  await api.accept(submittedId);
  await api.waitForState(submittedId, "closed");

  await page.goto(`ticket/${submittedId}`);
  await expect(page.getByTestId("ticket-view")).toBeVisible();
  await expect(page.locator('[data-testid^="action-"]')).toHaveCount(0);
});
