import { useSyncExternalStore } from "react";
import { getSnapshot, subscribe } from "../store";
import { App } from "./App";

/// Mounted once. Every later update arrives through the store, so the keypress listeners
/// registered by `App` and `Modal` are registered once too.
export function Root() {
  const props = useSyncExternalStore(subscribe, getSnapshot);
  return <App {...props} />;
}
