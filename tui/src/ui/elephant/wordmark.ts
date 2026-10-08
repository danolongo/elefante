/// The "elefante" wordmark shown under the intro elephant, from elefante_text1.png: one pixel is one
/// column by half a row, like the sprites, so it is 61 columns by 5 rows. `L` is lit, `.` transparent.
export const LOGO: string[] = [
  "........LLL..............LLL.................................",
  "..........L.............L...L...................L............",
  "..........L.............L...L...................L............",
  ".LLLLL....L....LLLLL....L......LLLLL..L.LLLL..LLLLLL...LLLLL.",
  "L.....L...L...L.....L...L...........L.LL....L...L.....L.....L",
  "L.....L...L...L.....L.LLLLL.........L.L.....L...L.....L.....L",
  "LLLLLLL...L...LLLLLLL...L......LLLLLL.L.....L...L.....LLLLLLL",
  "L.........L...L.........L.....L.....L.L.....L...L.....L......",
  "L.........L...L.........L.....L....LL.L.....L...L...L.L......",
  ".LLLLL..LLLLL..LLLLL....L......LLLL.L.L.....L....LLL...LLLLL.",
];
export const LOGO_COLS = LOGO[0].length;
export const LOGO_ROWS = LOGO.length / 2;
