import { RGBA, type OptimizedBuffer } from "@opentui/core";
import type { Frame } from "./scenes";
import { PALETTE, sprite } from "./sprites";

export type Stage = {
  cols: number;
  /// Stage height in pixels; two pixel rows make one terminal row.
  pixelRows: number;
  /// Pixel row the sprite's top edge sits on.
  top: number;
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
  sprite(frame.pose).forEach((row, r) => {
    for (let c = 0; c < row.length; c++) {
      const x = frame.x + c, y = stage.top + r;
      if (row[c] !== "." && x >= 0 && x < stage.cols && y < stage.pixelRows) pixels[y][x] = row[c];
    }
  });
  return pixels;
}

/// Paints pixels with half-block cells: `▀` carries the top pixel in its foreground and the
/// bottom pixel in its background, and a lone bottom pixel is `▄`. Pixels with nothing behind
/// them stay transparent so the transcript shows through around the elephant.
export function paintPixels(buffer: OptimizedBuffer, pixels: (string | null)[][]): void {
  buffer.clear(TRANSPARENT);
  for (let row = 0; row < pixels.length; row += 2) {
    for (let col = 0; col < pixels[row].length; col++) {
      const top = pixels[row][col], bottom = pixels[row + 1]?.[col] ?? null;
      if (top) buffer.setCellWithAlphaBlending(col, row / 2, "▀", color(top), bottom ? color(bottom) : TRANSPARENT);
      else if (bottom) buffer.setCellWithAlphaBlending(col, row / 2, "▄", color(bottom), TRANSPARENT);
    }
  }
}

export const paintFrame = (buffer: OptimizedBuffer, frame: Frame, stage: Stage): void => paintPixels(buffer, composePixels(frame, stage));
