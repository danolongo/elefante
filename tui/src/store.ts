import type { AppProps } from "./ui/App";

/// The view model the renderer reads, and the subscription that tells it to.
///
/// `root.render()` remounts the tree on every call rather than reconciling, so calling it per
/// update leaks a keypress listener each time. The root is rendered exactly once instead, and
/// updates arrive through this store.
let snapshot: AppProps | null = null;
const listeners = new Set<() => void>();

/// Publishes a new view. The object identity must change for React to see the update, which
/// is why callers build a fresh one rather than mutating.
export function publish(next: AppProps): void {
  snapshot = next;
  for (const listener of listeners) listener();
}

export function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

/// Returns the cached snapshot, never a fresh object — `useSyncExternalStore` compares by
/// identity and would loop forever on a new one each call.
export function getSnapshot(): AppProps {
  if (snapshot === null) throw new Error("store read before the first publish()");
  return snapshot;
}
