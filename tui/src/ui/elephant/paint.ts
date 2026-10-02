import { RGBA, type OptimizedBuffer } from "@opentui/core";
import type { Frame } from "./scenes";
import { PALETTE, sprite } from "./sprites";

export type Stage = {
  cols: number;
  /// Stage height in pixels; two pixel rows make one terminal row.
  pixelRows: number;
  /// Pixel row the sprite's top edge sits on.
  top: number;
  /// Dotted ground on the last pixel row, as in the design.
  ground: boolean;
};

const TRANSPARENT = RGBA.fromValues(0, 0, 0, 0);
const colors = new Map<string, RGBA>();
const color = (pixel: string): RGBA => {
  let value = colors.get(pixel);
  if (!value) { value = RGBA.fromHex(PALETTE[pixel]); colors.set(pixel, value); }
  return value;
};

/// Composes a frame into pixels. `null` is transparent.
export function composePixels(frame: Frame, stage: Stage): (string | null)[][] {
  const pixels = Array.from({ length: stage.pixelRows }, () => Array<string | null>(stage.cols).fill(null));
  if (stage.ground) for (let col = 0; col < stage.cols; col += 2) pixels[stage.pixelRows - 1][col] = "G";
  sprite(frame.pose).forEach((row, r) => {
    for (let c = 0; c < row.length; c++) {
      const x = frame.x + c, y = stage.top + r;
      if (row[c] !== "." && x >= 0 && x < stage.cols && y < stage.pixelRows) pixels[y][x] = row[c];
    }
  });
  return pixels;
}

/// Paints a frame with half-block cells: `▀` carries the top pixel in its foreground and the
/// bottom pixel in its background, and a lone bottom pixel is `▄`. Pixels with nothing behind
/// them stay transparent so the transcript shows through around the elephant.
export function paintFrame(buffer: OptimizedBuffer, frame: Frame, stage: Stage): void {
  buffer.clear(TRANSPARENT);
  const pixels = composePixels(frame, stage);
  for (let row = 0; row < stage.pixelRows; row += 2) {
    for (let col = 0; col < stage.cols; col++) {
      const top = pixels[row][col], bottom = pixels[row + 1]?.[col] ?? null;
      if (top) buffer.setCellWithAlphaBlending(col, row / 2, "▀", color(top), bottom ? color(bottom) : TRANSPARENT);
      else if (bottom) buffer.setCellWithAlphaBlending(col, row / 2, "▄", color(bottom), TRANSPARENT);
    }
  }
}
