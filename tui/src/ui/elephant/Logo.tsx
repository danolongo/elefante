import { useEffect, useRef } from "react";
import type { FrameBufferRenderable } from "@opentui/core";
import { useRenderer } from "@opentui/react";
import { paintPixels } from "./paint";
import { LOGO, LOGO_COLS, LOGO_ROWS } from "./wordmark";

const pixels = LOGO.map(row => [...row].map(c => (c === "." ? null : c)));

/// The wordmark: static, so it is painted once and costs nothing after that.
export function Logo() {
  const renderer = useRenderer();
  const buffer = useRef<FrameBufferRenderable>(null);
  useEffect(() => {
    if (!buffer.current) return;
    paintPixels(buffer.current.frameBuffer, pixels);
    renderer.requestRender();
  }, []);
  return <frame-buffer id="elefante-logo" ref={buffer} respectAlpha width={LOGO_COLS} height={LOGO_ROWS} />;
}
