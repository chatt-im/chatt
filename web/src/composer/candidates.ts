import type { CandidateKind } from "../types";

// Browser-local request correlation for autocomplete data. Request ids are
// monotonic for the lifetime of the page, while invalidation clears which ids
// may still update the current room/connection cache.
export class CandidateRequestTracker {
  private nextRequestId = 1;
  private readonly pending = new Map<CandidateKind, number>();

  begin(kind: CandidateKind): number {
    const requestId = this.nextRequestId++;
    this.pending.set(kind, requestId);
    return requestId;
  }

  accept(kind: CandidateKind, requestId: number): boolean {
    if (this.pending.get(kind) !== requestId) return false;
    this.pending.delete(kind);
    return true;
  }

  invalidate(): void {
    this.pending.clear();
  }
}
