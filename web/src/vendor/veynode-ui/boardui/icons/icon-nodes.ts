import { DRAWINGS } from "../../icons/drawings";

/**
 * The lucide drawings the animated icons are made of, copied from the
 * lucide-react release the app ships (0.577.0) so that an AnimatedIcon at
 * rest draws exactly what the plain lucide icon draws
 * (packages/ui/tests/unit/animated-icon.test.tsx compares them).
 * Each entry is [tag, attributes], in lucide's own order. An icon Veynode
 * redrew (`icons/drawings.ts`) is drawn from there instead.
 */
export type IconNodeElement = readonly [
  tag: "path" | "rect" | "circle" | "line",
  attributes: Readonly<Record<string, string>>,
];
export type IconNode = readonly IconNodeElement[];

const path = (d: string): IconNodeElement => ["path", { d }];

export const ICON_NODES = {
  "arrow-up": [path("m5 12 7-7 7 7"), path("M12 19V5")],
  bell: [
    path("M10.268 21a2 2 0 0 0 3.464 0"),
    path(
      "M3.262 15.326A1 1 0 0 0 4 17h16a1 1 0 0 0 .74-1.673C19.41 13.956 18 12.499 18 8A6 6 0 0 0 6 8c0 4.499-1.411 5.956-2.738 7.326",
    ),
  ],
  check: [path("M20 6 9 17l-5-5")],
  "chevron-down": [path("m6 9 6 6 6-6")],
  "chevron-right": [path("m9 18 6-6-6-6")],
  "chevron-up": [path("m18 15-6-6-6 6")],
  copy: [
    ["rect", { width: "14", height: "14", x: "8", y: "8", rx: "2", ry: "2" }],
    path("M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"),
  ],
  download: [
    path("M12 15V3"),
    path("M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"),
    path("m7 10 5 5 5-5"),
  ],
  "external-link": [
    path("M15 3h6v6"),
    path("M10 14 21 3"),
    path("M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"),
  ],
  eye: [
    path(
      "M2.062 12.348a1 1 0 0 1 0-.696 10.75 10.75 0 0 1 19.876 0 1 1 0 0 1 0 .696 10.75 10.75 0 0 1-19.876 0",
    ),
    ["circle", { cx: "12", cy: "12", r: "3" }],
  ],
  folder: DRAWINGS.folder,
  "folder-open": [
    path(
      "m6 14 1.5-2.9A2 2 0 0 1 9.24 10H20a2 2 0 0 1 1.94 2.5l-1.54 6a2 2 0 0 1-1.95 1.5H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h3.9a2 2 0 0 1 1.69.9l.81 1.2a2 2 0 0 0 1.67.9H18a2 2 0 0 1 2 2v2",
    ),
  ],
  languages: [
    path("m5 8 6 6"),
    path("m4 14 6-6 2-3"),
    path("M2 5h12"),
    path("M7 2h1"),
    path("m22 22-5-10-5 10"),
    path("M14 18h6"),
  ],
  "loader-circle": [path("M21 12a9 9 0 1 1-6.219-8.56")],
  lock: [
    ["rect", { width: "18", height: "11", x: "3", y: "11", rx: "2", ry: "2" }],
    path("M7 11V7a5 5 0 0 1 10 0v4"),
  ],
  menu: [path("M4 5h16"), path("M4 12h16"), path("M4 19h16")],
  mic: [
    path("M12 19v3"),
    path("M19 10v2a7 7 0 0 1-14 0v-2"),
    ["rect", { x: "9", y: "2", width: "6", height: "13", rx: "3" }],
  ],
  moon: [
    path(
      "M20.985 12.486a9 9 0 1 1-9.473-9.472c.405-.022.617.46.402.803a6 6 0 0 0 8.268 8.268c.344-.215.825-.004.803.401",
    ),
  ],
  "panel-right": [
    ["rect", { width: "18", height: "18", x: "3", y: "3", rx: "2" }],
    path("M15 3v18"),
  ],
  pause: [
    ["rect", { x: "14", y: "3", width: "5", height: "18", rx: "1" }],
    ["rect", { x: "5", y: "3", width: "5", height: "18", rx: "1" }],
  ],
  pin: [
    path("M12 17v5"),
    path(
      "M9 10.76a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V16a1 1 0 0 0 1 1h12a1 1 0 0 0 1-1v-.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.76V7a1 1 0 0 1 1-1 2 2 0 0 0 0-4H8a2 2 0 0 0 0 4 1 1 0 0 1 1 1z",
    ),
  ],
  play: [
    path(
      "M5 5a2 2 0 0 1 3.008-1.728l11.997 6.998a2 2 0 0 1 .003 3.458l-12 7A2 2 0 0 1 5 19z",
    ),
  ],
  plus: [path("M5 12h14"), path("M12 5v14")],
  power: [path("M12 2v10"), path("M18.4 6.6a9 9 0 1 1-12.77.04")],
  "refresh-cw": [
    path("M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"),
    path("M21 3v5h-5"),
    path("M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"),
    path("M8 16H3v5"),
  ],
  reply: [path("M20 18v-2a4 4 0 0 0-4-4H4"), path("m9 17-5-5 5-5")],
  "rotate-ccw": [
    path("M3 12a9 9 0 1 0 9-9 9.75 9.75 0 0 0-6.74 2.74L3 8"),
    path("M3 3v5h5"),
  ],
  "rotate-cw": [
    path("M21 12a9 9 0 1 1-9-9c2.52 0 4.93 1 6.74 2.74L21 8"),
    path("M21 3v5h-5"),
  ],
  search: [path("m21 21-4.34-4.34"), ["circle", { cx: "11", cy: "11", r: "8" }]],
  settings: DRAWINGS.settings,
  sparkles: [
    path(
      "M11.017 2.814a1 1 0 0 1 1.966 0l1.051 5.558a2 2 0 0 0 1.594 1.594l5.558 1.051a1 1 0 0 1 0 1.966l-5.558 1.051a2 2 0 0 0-1.594 1.594l-1.051 5.558a1 1 0 0 1-1.966 0l-1.051-5.558a2 2 0 0 0-1.594-1.594l-5.558-1.051a1 1 0 0 1 0-1.966l5.558-1.051a2 2 0 0 0 1.594-1.594z",
    ),
    path("M20 2v4"),
    path("M22 4h-4"),
    ["circle", { cx: "4", cy: "20", r: "2" }],
  ],
  sun: [
    ["circle", { cx: "12", cy: "12", r: "4" }],
    path("M12 2v2"),
    path("M12 20v2"),
    path("m4.93 4.93 1.41 1.41"),
    path("m17.66 17.66 1.41 1.41"),
    path("M2 12h2"),
    path("M20 12h2"),
    path("m6.34 17.66-1.41 1.41"),
    path("m19.07 4.93-1.41 1.41"),
  ],
  "trash-2": [
    path("M10 11v6"),
    path("M14 11v6"),
    path("M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"),
    path("M3 6h18"),
    path("M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"),
  ],
  upload: [
    path("M12 3v12"),
    path("m17 8-5-5-5 5"),
    path("M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"),
  ],
  video: [
    path(
      "m16 13 5.223 3.482a.5.5 0 0 0 .777-.416V7.87a.5.5 0 0 0-.752-.432L16 10.5",
    ),
    ["rect", { x: "2", y: "6", width: "14", height: "12", rx: "2" }],
  ],
  "volume-2": [
    path(
      "M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298z",
    ),
    path("M16 9a5 5 0 0 1 0 6"),
    path("M19.364 18.364a9 9 0 0 0 0-12.728"),
  ],
} as const satisfies Record<string, IconNode>;

export type IconNodeName = keyof typeof ICON_NODES;
