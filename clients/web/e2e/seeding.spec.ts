// Checks the seeding helpers themselves, so a spec built on them can trust the state it starts
// from: each seeded ticket reaches its state on the server and shows in the right group.
import { expect, test } from "./fixtures";

test("seedSubmitted lands in Ready for review with its evidence", async ({ page, api }) => {
  const seeded = await api.seedSubmitted("Seeding: a submitted ticket");
  const ticket = await api.getTicket(seeded.id);
  expect(ticket.state).toBe("submitted");

  const state = (await api.getState()) as { evidence: { ticket: string; artifact: string }[] };
  expect(state.evidence).toContainEqual(
    expect.objectContaining({ ticket: seeded.id, artifact: seeded.artifact }),
  );

  await page.goto("");
  await expect(page.getByTestId("group-review").getByTestId(`row-${seeded.id}`)).toBeVisible();
});

test("seedEscalated lands in Needs input after three attempts", async ({ page, api }) => {
  const id = await api.seedEscalated("Seeding: an escalated ticket");
  expect((await api.getTicket(id)).attempts).toBe(3);

  await page.goto("");
  await expect(page.getByTestId("group-needs_input").getByTestId(`row-${id}`)).toBeVisible();
});

test("createTicket, cancel and waitForState", async ({ page, api }) => {
  const id = await api.createTicket("Seeding: a draft to cancel");
  expect((await api.getTicket(id)).state).toBe("draft");
  await api.cancel(id, "e2e");
  await api.waitForState(id, "cancelled");

  await page.goto(`ticket/${id}`);
  await expect(page.getByText("Seeding: a draft to cancel").first()).toBeVisible();
});

test("freshServer starts an empty project", async ({ page, freshServer }) => {
  await page.goto(freshServer.appURL);
  await expect(page.getByTestId("tickets-counts")).toHaveText("No tickets yet");
});
