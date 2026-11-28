/** Shared Chrome extension runtime effects and response algebra. */

export { errorMessage } from "./webview_protocol.js";

/** Standard response envelope returned through chrome.runtime messaging. */
export type RuntimeResponse<T> =
  | { readonly ok: true; readonly result?: T }
  | { readonly ok: false; readonly error: string };

/** Failure-tolerant FIFO for one class of serialized extension effects. */
export type SerializedEffectQueue = {
  readonly enqueue: (effect: () => Promise<void>) => Promise<void>;
};

/** Creates an independent queue whose failed effect cannot poison later work. */
export function createSerializedEffectQueue(): SerializedEffectQueue {
  let tail: Promise<void> = Promise.resolve();
  return {
    enqueue: (effect: () => Promise<void>): Promise<void> => {
      const queued = tail.then(effect, effect);
      tail = queued.catch((): void => {
        // The initiating caller observes the error; later independent effects still run.
      });
      return queued;
    },
  };
}
