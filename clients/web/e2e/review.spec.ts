// The Review page (`src/views/ReviewView.tsx`): it lists exactly the submitted tickets (oldest
// first), the nav badge count matches, inline Accept/Reject work and an actioned item leaves the
// list live, and it shows an empty state when nothing is waiting.
//
// The worker's shared `tmServer` accumulates tickets across every test that worker runs (see
// e2e/README.md), so every assertion here is scoped by ticket id (never a total or "the first
// row") and counts are always derived from a fresh `api.listTickets()` call, never hardcoded.
import { expect, test, type Ticket } from "./fixtures";

async function submittedIds(api: { listTickets(): Promise<Ticket[]> }): Promise<Set<string>> {
  const tickets = await api.listTickets();
  return new Set(tickets.filter((t) => t.state === "submitted").map((t) => t.id));
}

test("lists exactly the tickets currently submitted, with their summary and evidence", async ({
  page,
  api,
}, testInfo) => {
  const objective = `Review list: a submission to show, run ${testInfo.testId}-${testInfo.repeatEachIndex}`;
  const seeded = await api.seedSubmitted(objective, { summary: `Reviewed summary for ${objective}` });

  // A draft never becomes submitted, so it must never show as a review row.
  const draftObjective = `Review list: a draft that must not appear, run ${testInfo.testId}-${testInfo.repeatEachIndex}`;
  const draftId = await api.createTicket(draftObjective);

  const expected = await submittedIds(api);
  expect(expected.has(seeded.id)).toBe(true);
  expect(expected.has(draftId)).toBe(false);

  await page.goto("review");
  await expect(page.getByTestId("review-view")).toBeVisible();

  const row = page.getByTestId(`review-${seeded.id}`);
  await expect(row).toBeVisible();
  await expect(row).toContainText(seeded.id);
  await expect(row).toContainText(`Reviewed summary for ${objective}`);
  await expect(row).toContainText(seeded.artifact);

  await expect(page.getByTestId(`review-${draftId}`)).toHaveCount(0);

  // Exactly the submitted tickets: every rendered review row's id is a currently-submitted
  // ticket, and every currently-submitted ticket has a rendered row.
  const rowIds = await page
    .locator("ul.reviews > li.review")
    .evaluateAll((nodes) => nodes.map((n) => (n as HTMLElement).dataset.testid?.replace(/^review-/, "")));
  expect(new Set(rowIds)).toEqual(expected);

  // Clean up so this submission doesn't linger into later tests' "exactly" checks.
  await api.accept(seeded.id);
});

test("the nav badge count matches the number of submitted tickets", async ({ page, api }, testInfo) => {
  const objective = `Review badge: count check, run ${testInfo.testId}-${testInfo.repeatEachIndex}`;
  const seeded = await api.seedSubmitted(objective);
  const expectedCount = (await submittedIds(api)).size;
  expect(expectedCount).toBeGreaterThan(0);

  await page.goto("");
  await expect(page.getByTestId("review-count")).toHaveText(String(expectedCount));

  await page.goto("review");
  await expect(page.getByTestId("review-count")).toHaveText(String(expectedCount));

  await api.accept(seeded.id);

  const afterCount = (await submittedIds(api)).size;
  await expect
    .poll(async () => {
      const el = page.getByTestId("review-count");
      return (await el.count()) === 0 ? "0" : await el.textContent();
    })
    .toBe(afterCount === 0 ? "0" : String(afterCount));
});

test("inline Accept closes the ticket and it leaves the review list live", async ({ page, api }, testInfo) => {
  const objective = `Review accept: run ${testInfo.testId}-${testInfo.repeatEachIndex}`;
  const seeded = await api.seedSubmitted(objective);

  await page.goto("review");
  const row = page.getByTestId(`review-${seeded.id}`);
  await expect(row).toBeVisible();

  await row.getByTestId("action-accept").click();

  // Leaves the list without a reload (the store is SSE-driven).
  await expect(row).toHaveCount(0);

  await expect.poll(async () => (await api.getTicket(seeded.id)).state).toBe("closed");
});

test("inline Reject requires a reason and sends the ticket back", async ({ page, api }, testInfo) => {
  const objective = `Review reject: run ${testInfo.testId}-${testInfo.repeatEachIndex}`;
  const seeded = await api.seedSubmitted(objective);

  await page.goto("review");
  const row = page.getByTestId(`review-${seeded.id}`);
  await expect(row).toBeVisible();

  await row.getByTestId("action-reject").click();

  const send = row.getByRole("button", { name: "Send back" });
  await expect(send).toBeDisabled();

  const reason = `Missing coverage for the empty-input case, run ${testInfo.testId}-${testInfo.repeatEachIndex}`;
  await row.getByRole("textbox").fill(reason);
  await expect(send).toBeEnabled();
  await send.click();

  // Leaves the list without a reload; the ticket read `ready` immediately, per the API helper's
  // own doc comment, then the mock worker escalates it on its own.
  await expect(row).toHaveCount(0);

  await expect.poll(async () => (await api.getTicket(seeded.id)).state).not.toBe("submitted");
});

test("shows the empty state when nothing is waiting for review", async ({ page, freshServer }) => {
  // A dedicated, empty project: no submitted tickets exist here at all, so this doesn't need to
  // filter around anything the shared worker server has accumulated.
  await page.goto(`${freshServer.appURL}review`);

  await expect(page.getByTestId("review-view")).toBeVisible();
  await expect(page.getByText("You're all caught up")).toBeVisible();
  await expect(page.getByText("Nothing is waiting for review")).toBeVisible();
  await expect(page.locator("ul.reviews > li.review")).toHaveCount(0);
  await expect(page.getByTestId("review-count")).toHaveCount(0);
});
