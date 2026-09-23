import { describe, expect, it } from "vitest";
import { event } from "../test/fixtures";
import { describeEvent } from "./timelineModel";

describe("describeEvent", () => {
  it("reads a submission and an escalation as sentences", () => {
    expect(describeEvent(event({ kind: "ticket.submitted", payload: { ticket: "T-1", summary: "did\nit" } })).text).toBe(
      "Submitted: did it",
    );
    const esc = describeEvent(event({ kind: "ticket.escalated", payload: { ticket: "T-1", reason: "out of attempts" } }));
    expect(esc).toMatchObject({ text: "Escalated: out of attempts", tone: "bad", minor: false });
  });

  it("drops the colon when the event carried no text", () => {
    expect(describeEvent(event({ kind: "ticket.verification_failed", payload: { ticket: "T-1", reason: "" } })).text).toBe(
      "Rejected",
    );
  });

  it("marks bookkeeping as minor", () => {
    expect(describeEvent(event({ kind: "usage.recorded", payload: { tokens: 12 } })).minor).toBe(true);
    expect(describeEvent(event({ kind: "ticket.state_changed" })).text).toBe("ready → leased");
  });
});
