import { useCallback, useEffect, useRef } from "react";
import { engine, FrameBufferRenderable, Timeline } from "@opentui/core";
import { extend, useRenderer } from "@opentui/react";
import { paintFrame, type Stage } from "./paint";
import { exitFrames, flapFrames, introFrames, nextFlapDelayMs, perchEnterFrames, perchFrame, TICK_MS, type Frame } from "./scenes";
import { SPRITE_COLS } from "./sprites";

extend({ "frame-buffer": FrameBufferRenderable });
declare module "@opentui/react" {
  interface OpenTUIComponents { "frame-buffer": typeof FrameBufferRenderable }
}

export const PERCH_COLS = 30;
export const topStage = (cols: number): Stage => ({ cols, pixelRows: 12, top: 1, ground: true });
export const perchStage: Stage = { cols: PERCH_COLS, pixelRows: 10, top: 0, ground: false };
const rows = (stage: Stage) => stage.pixelRows / 2;

/// Plays clips into a frame buffer on an OpenTUI timeline.
///
/// The timeline tweens a tick counter and `onUpdate` paints the frame under it straight into the
/// buffer, so a clip causes no React renders. The engine keeps the renderer live only while a
/// timeline is playing, which is why a standing elephant costs nothing.
function usePlayer(stage: Stage) {
  const renderer = useRenderer();
  const buffer = useRef<FrameBufferRenderable>(null);
  const running = useRef<Timeline | null>(null);
  const shown = useRef<Frame | null>(null);
  const latest = useRef(stage);
  latest.current = stage;

  const show = useCallback((frame: Frame) => {
    shown.current = frame;
    if (!buffer.current) return;
    paintFrame(buffer.current.frameBuffer, frame, latest.current);
    renderer.requestRender();
  }, [renderer]);

  const stop = useCallback(() => {
    const timeline = running.current;
    if (!timeline) return;
    running.current = null;
    timeline.pause();
    engine.unregister(timeline);
  }, []);

  const play = useCallback((frames: Frame[], done?: () => void) => {
    stop();
    const clock = { tick: 0 };
    let index = -1;
    // The timeline ends at its own `duration` (default one second), not when its tweens do.
    const timeline = new Timeline({
      autoplay: false, duration: frames.length * TICK_MS,
      onComplete: () => { if (running.current === timeline) { stop(); done?.(); } },
    });
    timeline.add(clock, {
      tick: frames.length, duration: frames.length * TICK_MS, ease: "linear",
      onUpdate: () => {
        const next = Math.min(frames.length - 1, Math.floor(clock.tick));
        if (next !== index) { index = next; show(frames[next]); }
      },
    });
    running.current = timeline;
    engine.register(timeline);
    timeline.play();
  }, [show, stop]);

  // A resize replaces the buffer's contents, so put the current frame back.
  useEffect(() => { if (shown.current) show(shown.current); }, [stage.cols, show]);
  useEffect(() => stop, [stop]);
  return { buffer, play, show, stop, shown };
}

export type TopElephantProps = {
  cols: number;
  phase: "intro" | "exit";
  onExited: () => void;
  /// Tests turn this off: the intro then shows its final pose and the exit completes at once.
  animate?: boolean;
};

/// New-session stage: walks in, raises its trunk, flaps and stands; on the first prompt walks off right.
export function TopElephant({ cols, phase, onExited, animate = true }: TopElephantProps) {
  const stage = topStage(cols);
  const { buffer, play, show, shown } = usePlayer(stage);
  const exited = useRef(onExited);
  exited.current = onExited;
  useEffect(() => {
    if (phase === "intro") {
      const frames = introFrames(cols);
      if (animate) play(frames); else show(frames[frames.length - 1]);
    } else if (animate) {
      const from = shown.current?.x ?? Math.round((cols - SPRITE_COLS) / 2);
      play(exitFrames(from, cols), () => exited.current());
    } else exited.current();
  }, [phase]);
  return <frame-buffer id="elephant-top" ref={buffer} respectAlpha width={cols} height={rows(stage)} />;
}

export type PerchElephantProps = {
  /// Walk in from the right edge, as after leaving the top stage; otherwise just stand there.
  arrive: boolean;
  animate?: boolean;
  /// Random source for the pause between flaps; injectable so tests need no 30-90 s wait.
  rng?: () => number;
};

/// Bottom-right, above the chat bar. Stands still, and flaps its ears once after each random 30-90 s pause.
export function PerchElephant({ arrive, animate = true, rng = Math.random }: PerchElephantProps) {
  const { buffer, play, show } = usePlayer(perchStage);
  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    const wait = () => { timer = setTimeout(() => play(flapFrames(PERCH_COLS), wait), nextFlapDelayMs(rng)); };
    if (!animate) show(perchFrame(PERCH_COLS));
    else if (arrive) play(perchEnterFrames(PERCH_COLS), wait);
    else { show(perchFrame(PERCH_COLS)); wait(); }
    return () => clearTimeout(timer);
  }, []);
  return <frame-buffer id="elephant-perch" ref={buffer} respectAlpha width={PERCH_COLS} height={rows(perchStage)} />;
}
