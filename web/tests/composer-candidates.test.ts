import { describe, expect, test } from "bun:test";
import { CandidateRequestTracker } from "../src/composer/candidates";

describe("candidate request correlation", () => {
  test("only the latest request for a kind may update the cache", () => {
    const requests = new CandidateRequestTracker();
    const stale = requests.begin("room");
    const current = requests.begin("room");
    expect(requests.accept("room", stale)).toBe(false);
    expect(requests.accept("room", current)).toBe(true);
    expect(requests.accept("room", current)).toBe(false);
  });

  test("room or connection invalidation rejects outstanding responses", () => {
    const requests = new CandidateRequestTracker();
    const requestId = requests.begin("user");
    requests.invalidate();
    expect(requests.accept("user", requestId)).toBe(false);
  });

  test("different candidate kinds are correlated independently", () => {
    const requests = new CandidateRequestTracker();
    const room = requests.begin("room");
    const sound = requests.begin("sound");
    expect(requests.accept("room", sound)).toBe(false);
    expect(requests.accept("room", room)).toBe(true);
    expect(requests.accept("sound", sound)).toBe(true);
  });
});
