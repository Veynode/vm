import type { IconNode, IconNodeElement } from "../boardui/icons/icon-nodes";

/**
 * Veynode's own line icons (ds 「图标」): the ones redrawn in place of the
 * lucide icon of the same name, on lucide's 24 grid and in its part order,
 * so an AnimatedIcon moves the same parts. `index.ts` makes each a component;
 * `boardui/icons/icon-nodes.ts` draws the animated ones from here.
 */
const path = (d: string): IconNodeElement => ["path", { d }];

/** A speech bubble with its tail at the lower left: the chats, and a Bot talking. */
export const BUBBLE =
  "M7 4h10a5 5 0 0 1 5 5v4a5 5 0 0 1-5 5h-6.2l-4.3 3.2a.6.6 0 0 1-1-.5v-3.1A5 5 0 0 1 2 13V9a5 5 0 0 1 5-5Z";
/** A folder with its tab. */
const FOLDER =
  "M4 4h4.6a2 2 0 0 1 1.6.8l1.3 1.7H20a2 2 0 0 1 2 2V18a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2Z";
/**
 * Three shapes of different kinds, a triangle over a square and a circle:
 * Space's place at the sidebar's foot, where the person's computers and
 * files are.
 */
export const SHAPES_TRIANGLE =
  "M12.76 3.89L15.84 9.11Q16.6 10.4 15.1 10.4H8.9Q7.4 10.4 8.16 9.11L11.24 3.89Q12 2.6 12.76 3.89Z";
export const SHAPES_SQUARE =
  "M5 13.5h4a2 2 0 0 1 2 2v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4a2 2 0 0 1 2-2Z";
export const SHAPES_CIRCLE = "M17.5 13.5a4 4 0 1 0 0 8a4 4 0 1 0 0-8Z";
/** The Bot's two eyes, as holes in a filled bubble. */
export const EYE_HOLES =
  "M9.5 8.7a.9.9 0 0 1 .9.9v2.8a.9.9 0 0 1-1.8 0V9.6a.9.9 0 0 1 .9-.9Z M14.5 8.7a.9.9 0 0 1 .9.9v2.8a.9.9 0 0 1-1.8 0V9.6a.9.9 0 0 1 .9-.9Z";

/** A cog of six round lobes, a closed curve sampled every 3.75 degrees. */
export const COG = (() => {
  const points: string[] = [];
  for (let step = 0; step < 96; step++) {
    const angle = (step / 96) * Math.PI * 2;
    const radius = 8.3 + 1.15 * Math.cos(6 * angle);
    points.push(
      `${(12 + radius * Math.sin(angle)).toFixed(2)} ${(12 - radius * Math.cos(angle)).toFixed(2)}`,
    );
  }
  return `M${points.join("L")}Z`;
})();
/** The cog's hole. */
export const COG_HOLE = "M12 9a3 3 0 1 0 0 6a3 3 0 1 0 0-6Z";

export const DRAWINGS = {
  "message-circle": [path(BUBBLE), path("M9.5 9.6v2.8"), path("M14.5 9.6v2.8")],
  settings: [path(COG), ["circle", { cx: "12", cy: "12", r: "3" }]],
  // One machine: a low box with a light, standing on a line.
  server: [
    path(
      "M5.5 6h13A3.5 3.5 0 0 1 22 9.5v4a3.5 3.5 0 0 1-3.5 3.5h-13A3.5 3.5 0 0 1 2 13.5v-4A3.5 3.5 0 0 1 5.5 6Z",
    ),
    path("M6.5 11.5h.01"),
    path("M17.5 11.5H11"),
    path("M7 20.5h10"),
  ],
  bot: [
    path("M12 8V5"),
    ["circle", { cx: "12", cy: "3.7", r: "1.3" }],
    path(
      "M6 8h12a3 3 0 0 1 3 3v6a3 3 0 0 1-3 3H6a3 3 0 0 1-3-3v-6a3 3 0 0 1 3-3Z",
    ),
    path("M9 13v2"),
    path("M15 13v2"),
  ],
  monitor: [
    path(
      "M4 4h16a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2Z",
    ),
    path("M12 17v3.5"),
    path("M8 20.5h8"),
  ],
  cpu: [
    path(
      "M7 5h10a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V7a2 2 0 0 1 2-2Z",
    ),
    path("M10 10h4v4h-4Z"),
    path("M9 2v3"),
    path("M15 2v3"),
    path("M9 19v3"),
    path("M15 19v3"),
    path("M2 9h3"),
    path("M2 15h3"),
    path("M19 9h3"),
    path("M19 15h3"),
  ],
  "messages-square": [
    path("M5 3h9a2 2 0 0 1 2 2v6a2 2 0 0 1-2 2H8l-5 3.5V5a2 2 0 0 1 2-2Z"),
    path("M19 9a2 2 0 0 1 2 2v10.5L17 18h-5a2 2 0 0 1-2-2"),
  ],
  folder: [path(FOLDER)],
  shapes: [path(SHAPES_TRIANGLE), path(SHAPES_SQUARE), path(SHAPES_CIRCLE)],
} satisfies Record<string, IconNode>;
