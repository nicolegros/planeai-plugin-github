import { JSDOM } from "jsdom";

/** A fresh browser-like document whose globals the UI entrypoints read when they mount. */
export function installDom(): JSDOM {
  const dom = new JSDOM("<!doctype html><html><body></body></html>", { url: "https://plugin.test/", pretendToBeVisual: true });
  const browser = dom.window as unknown as Record<string, unknown>;
  for (const name of ["window", "document", "Element", "HTMLElement", "HTMLButtonElement", "MutationObserver", "KeyboardEvent", "requestAnimationFrame"]) {
    Object.defineProperty(globalThis, name, { value: name === "window" ? dom.window : browser[name], configurable: true, writable: true });
  }
  return dom;
}

export type Deferred<T> = { promise: Promise<T>; resolve: (value: T) => void; reject: (error: unknown) => void };

export function deferred<T>(): Deferred<T> {
  let resolve: (value: T) => void = () => {};
  let reject: (error: unknown) => void = () => {};
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}

/** Lets pending promise callbacks and timers run. */
export const settle = () => new Promise((done) => setTimeout(done, 0));

export function press(key: string, target: Element = document.activeElement ?? document.body, modifiers: KeyboardEventInit = {}) {
  target.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true, ...modifiers }));
}
