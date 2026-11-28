/**
 * Pure navigation state machine for the trusted Extension WebView host.
 */

/** Immutable application-owned navigation history. */
type WebviewHistory = {
  readonly entries: readonly string[];
  readonly index: number;
};

/** History effect applied only when a renderer commit succeeds. */
export type NavigationIntent =
  | { readonly kind: "push" }
  | { readonly kind: "reload" }
  | { readonly kind: "history"; readonly index: number };

/** Observable phases of one WebView navigation transition. */
type NavigationPhase =
  | { readonly kind: "idle" }
  | {
      readonly kind: "fetching";
      readonly generation: number;
      readonly target: string;
      readonly intent: NavigationIntent;
    }
  | {
      readonly kind: "rendering";
      readonly generation: number;
      readonly target: string;
      readonly intent: NavigationIntent;
    }
  | { readonly kind: "committed"; readonly target: string }
  | { readonly kind: "failed"; readonly message: string };

/** Pure navigation model retained by the trusted host. */
export type NavigationState = {
  readonly generation: number;
  readonly committedTarget?: string;
  readonly history: WebviewHistory;
  readonly phase: NavigationPhase;
};

/** Returns the unique initial state. */
export function initialNavigationState(): NavigationState {
  return {
    generation: 0,
    history: { entries: [], index: -1 },
    phase: { kind: "idle" },
  };
}

/** Pure transition from each stable state into a newer fetching generation. */
export function beginNavigation(state: NavigationState, target: string, intent: NavigationIntent): NavigationState {
  const generation = state.generation + 1;
  return { ...state, generation, phase: { kind: "fetching", generation, target, intent } };
}

/** Pure transition from fetching into rendering for the same generation. */
export function beginRendering(state: NavigationState, generation: number, target: string): NavigationState {
  const phase = state.phase;
  if (phase.kind !== "fetching" || phase.generation !== generation) return state;
  return { ...state, phase: { kind: "rendering", generation, target, intent: phase.intent } };
}

/**
 * Pure commit transition that atomically updates target, phase, and history.
 *
 * Post: committedTarget == phase.target == history.entries[history.index].
 */
export function commitNavigation(state: NavigationState, generation: number): NavigationState {
  const phase = state.phase;
  if (phase.kind !== "rendering" || phase.generation !== generation) return state;
  const history = committedHistory(state.history, phase.target, phase.intent);
  return {
    ...state,
    committedTarget: phase.target,
    history,
    phase: { kind: "committed", target: phase.target },
  };
}

/** Pure failure transition that preserves the last committed target and history. */
export function failNavigation(state: NavigationState, generation: number, message: string): NavigationState {
  if (!isActiveNavigation(state, generation)) return state;
  return { ...state, phase: { kind: "failed", message } };
}

/** Returns whether a generation still owns the current effect continuation. */
export function isActiveNavigation(state: NavigationState, generation: number): boolean {
  return (
    state.generation === generation &&
    (state.phase.kind === "fetching" || state.phase.kind === "rendering") &&
    state.phase.generation === generation
  );
}

/** Applies one successful navigation intent to immutable history. */
function committedHistory(history: WebviewHistory, target: string, intent: NavigationIntent): WebviewHistory {
  if (intent.kind === "reload") {
    if (history.index < 0 || history.index >= history.entries.length) return appendedHistory(history, target);
    const entries = [...history.entries];
    entries[history.index] = target;
    return { entries, index: history.index };
  }
  if (intent.kind === "history") {
    const entries = [...history.entries];
    if (intent.index < 0 || intent.index >= entries.length) return appendedHistory(history, target);
    entries[intent.index] = target;
    return { entries, index: intent.index };
  }
  if (history.entries[history.index] === target) return history;
  return appendedHistory(history, target);
}

/** Appends one target after the currently committed history prefix. */
function appendedHistory(history: WebviewHistory, target: string): WebviewHistory {
  const entries = history.entries.slice(0, history.index + 1);
  entries.push(target);
  return { entries, index: entries.length - 1 };
}
