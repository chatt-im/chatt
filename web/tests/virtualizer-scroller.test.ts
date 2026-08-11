import { expect, test } from "bun:test";
import { createScroller } from "../src/vendor/virtua/core/scroller";
import {
  ACTION_ITEM_RESIZE,
  ACTION_VIEWPORT_RESIZE,
  createVirtualStore,
} from "../src/vendor/virtua/core/store";

class FakeViewport {
  scrollTop = 0;
  style: Record<string, string | undefined> = {};

  addEventListener() {}
  removeEventListener() {}
}

async function settleScheduler() {
  await Promise.resolve();
  await Promise.resolve();
}

function setupScroller() {
  const store = createVirtualStore(20, 40);
  store.$update(ACTION_VIEWPORT_RESIZE, 200);
  const scroller = createScroller(store, false);
  const viewport = new FakeViewport();
  scroller.$observe(
    {} as HTMLElement,
    viewport as unknown as HTMLElement,
  );
  return { scroller, store, viewport };
}

test("measurement resumes an imperative scroll while it is pending", async () => {
  const { scroller, store, viewport } = setupScroller();
  scroller.$scrollTo(700);
  await settleScheduler();
  expect(viewport.scrollTop).toBe(700);

  viewport.scrollTop = 400;
  store.$update(ACTION_ITEM_RESIZE, [[0, 80]]);
  await settleScheduler();

  expect(viewport.scrollTop).toBe(700);
  scroller.$cancelScroll?.();
  scroller.$dispose();
});

test("canceling prevents measurement from resuming a superseded scroll", async () => {
  const { scroller, store, viewport } = setupScroller();
  scroller.$scrollTo(700);
  await settleScheduler();
  expect(viewport.scrollTop).toBe(700);

  viewport.scrollTop = 400;
  scroller.$cancelScroll();
  store.$update(ACTION_ITEM_RESIZE, [[0, 80]]);
  await settleScheduler();

  expect(viewport.scrollTop).toBe(400);
  scroller.$dispose();
});
