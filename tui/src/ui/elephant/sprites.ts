/// Pixel art for the mascot, ported from the "8-bit Elephant TUI Animations" design project.
/// One character is one pixel; `.` is transparent. Two pixel rows share one terminal row.
export const PALETTE: Record<string, string> = { A: "#464646", B: "#b4b4b4", C: "#000000" };

export type Ear = "n" | "s" | "o";
export type Trunk = "down" | "mid" | "up";
export type Pose = { ear?: Ear; trunk?: Trunk; leg?: number; flip?: boolean };

export const SPRITE_COLS = 15;
export const SPRITE_ROWS = 10;

const HEAD: Record<Ear, string[]> = {
  n: ["......AABB", "....BAAABBB", "...BBAAABCB", "..BBBBAABBB"],
  s: [".....AAABB", "....AAAABBB", "...BBAAABCB", "..BBBBAABBB"],
  o: ["....AAAABB", "...AAAAABBB", "..AAAAAABCB", "..BAAAAABBB"],
};
const BODY: Record<Trunk, string[]> = {
  down: [".BBBBBBBBBBB", "BBBBBBBBB.BBB", "..BBBBBBB...B"],
  mid: [".BBBBBBBBBBBB", "BBBBBBBBB", "..BBBBBBB"],
  up: [".BBBBBBBBBB", "BBBBBBBBB", "..BBBBBBB"],
};
/// `[row, col]` pixels of the raised trunk, in sprite coordinates.
const TRUNK: Record<Trunk, [number, number][]> = { down: [], mid: [[4, 13]], up: [[4, 11], [3, 11], [2, 12], [1, 12], [0, 13]] };
const LEGS = [["..BB...BB", "..AA...AA"], ["..BB...BB", ".AA.....AA"], ["..BB...BB", "..AA...AA"], ["..BB...BB", "...AA.AA"]];

const cache = new Map<string, string[]>();

function put(grid: string[][], row: number, text: string) {
  for (let col = 0; col < text.length; col++) if (text[col] !== ".") grid[row][col] = text[col];
}

/// The elephant's pixel rows for a pose, right-facing unless `flip`. Standing legs (0) and the
/// design's stride poses (1 and 3) are distinct; even indices are the same standing pose.
export function sprite({ ear = "n", trunk = "down", leg = 0, flip = false }: Pose = {}): string[] {
  const stride = leg % 2 ? leg % 4 : 0;
  const key = `${trunk}_${ear}_${stride}`;
  let rows = cache.get(key);
  if (!rows) {
    const grid = Array.from({ length: SPRITE_ROWS }, () => Array<string>(SPRITE_COLS).fill("."));
    HEAD[ear].forEach((text, i) => put(grid, 1 + i, text));
    BODY[trunk].forEach((text, i) => put(grid, 5 + i, text));
    TRUNK[trunk].forEach(([row, col]) => { grid[row][col] = "B"; });
    LEGS[stride].forEach((text, i) => put(grid, 8 + i, text));
    rows = grid.map(row => row.join(""));
    cache.set(key, rows);
  }
  return flip ? rows.map(row => [...row].reverse().join("")) : rows;
}
