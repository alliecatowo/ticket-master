import { expect, test } from "./fixtures";

test("the home screen loads and a dispatched task becomes a row", async ({ page, api }, testInfo) => {
  // `baseURL` is the server's /app/ URL, so "" is the home screen. A leading slash ("/") would
  // resolve to the API origin instead.
  await page.goto("");
  await expect(page.getByRole("heading", { level: 1, name: "Tickets" })).toBeVisible();

  const objective = `Dispatch from the home screen, run ${testInfo.testId}-${testInfo.repeatEachIndex}`;
  const input = page.getByTestId("dispatch-input");
  await input.fill(objective);
  await input.press("Enter");

  // The server has the ticket, and the dispatch box activated it (it left draft).
  let id = "";
  await expect
    .poll(async () => {
      const ticket = (await api.listTickets()).find((t) => t.objective === objective);
      id = ticket?.id ?? "";
      return ticket?.state ?? "missing";
    })
    .not.toMatch(/^(missing|draft)$/);

  // Its row shows, found by id rather than by group: the mock worker moves it from Queued to
  // Needs input within seconds. A row's title is a shortened objective, so match on its start.
  const row = page.getByTestId(`row-${id}`);
  await expect(row).toBeVisible();
  await expect(row).toContainText(id);
  await expect(row).toContainText("Dispatch from the home screen");
  await expect(input).toHaveValue("");
});
