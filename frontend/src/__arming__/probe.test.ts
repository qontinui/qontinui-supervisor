import { expect, test } from "vitest";

export function armingProbe(x: boolean): void {
  if (x) {}
}

test("arming probe must fail", () => {
  expect(1).toBe(2);
});
