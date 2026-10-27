export const avatarIcons = [
  "monitor",
  "laptop",
  "vm",
  "box",
  "server",
  "terminal",
  "cpu",
  "globe",
  "folder",
  "code",
] as const;
export const avatarShapes = [
  "round",
  "oval",
  "square",
  "capsule",
  "triangle",
  "hexagon",
  "cloud",
  "drop",
] as const;
/**
 * The Bot colours, in the picker's order. A Bot's colour is its identity: the
 * interface derives its tints from these eight and from nothing else.
 */
export const avatarColors = [
  "#328cff",
  "#20bdb0",
  "#00c879",
  "#ffb337",
  "#ff7a18",
  "#f653b5",
  "#a576ff",
  "#18181b",
] as const;
export type AvatarColor = (typeof avatarColors)[number];
/**
 * The avatar colours by name, in `avatarColors` order: what a Bot choosing a
 * teammate's look can say, instead of a hex it would have to guess.
 */
export const lookColors = ["blue", "teal", "green", "yellow", "orange", "pink", "purple", "black"] as const;
export type AvatarColorName = (typeof lookColors)[number];

/** The screen-face heads: one face (a screen with two eyes) on eight silhouettes. */
export const faceHeads = ["soft", "round", "pill", "egg", "dome", "wide", "box", "cloud"] as const;
export type FaceHead = (typeof faceHeads)[number];
/** One accessory a head wears: on top of it, or at its sides (ears, headphones, glasses). */
export const faceAccessories = [
  "none",
  "antenna",
  "twin",
  "sprout",
  "leaf",
  "cat",
  "beret",
  "propeller",
  "bow",
  "ears",
  "headphones",
  "glasses",
] as const;
export type FaceAccessory = (typeof faceAccessories)[number];
export type FaceAvatar = { head: FaceHead; accessory: FaceAccessory; color: AvatarColor };

/** The six robot heads of the previous release (`robot:<shape>:<color>`). Still valid on stored records. */
export const legacyRobotShapes = ["round", "squircle", "dome", "wide", "antenna", "ears"] as const;
/** Earlier Moodstone cuts. Still valid on stored records; drawn as the nearest head. */
export const moodstoneCuts = ["circle", "squircle", "square", "diamond", "hexagon", "badge", "burst"] as const;

/** `robot:<head>:<accessory>:<color>`. */
export const faceAvatarValue = ({ head, accessory, color }: FaceAvatar) => `robot:${head}:${accessory}:${color}`;
export const defaultAvatar = "robot:soft:antenna:#328cff";
/** Defaults earlier releases stored; a record holding one never chose it. */
const legacyDefaultAvatars = ["robot:round:#328cff", "moodstone:circle:#3FC1F5:#FFFFFF"];
export function isDefaultAvatar(value: unknown) {
  return value === defaultAvatar || legacyDefaultAvatars.some((avatar) => avatar === value);
}
const includes = <T extends string>(list: readonly T[], value: unknown): value is T =>
  list.some((item) => item === value);
const isHex = (value: string | undefined) => /^#[0-9a-f]{6}$/i.test(value ?? "");
export function isAvatar(value: unknown): value is string {
  if (typeof value !== "string") return false;
  if (/^data:image\/(png|jpeg|webp);base64,[A-Za-z0-9+/]+={0,2}$/.test(value))
    return value.length <= 350000;
  const parts = value.split(":");
  const [prefix, shape, second, third] = parts;
  if (prefix === "robot")
    return parts.length === 4
      ? includes(faceHeads, shape) && includes(faceAccessories, second) && isHex(third)
      : parts.length === 3 && includes(legacyRobotShapes, shape) && isHex(second);
  if (prefix === "moodstone")
    return parts.length === 4 && includes(moodstoneCuts, shape) && isHex(second) &&
      (third === "#FFFFFF" || third === "#111111");
  return (
    ((prefix === "bot" && includes(avatarShapes, shape)) ||
      (prefix === "icon" && includes(avatarIcons, shape))) &&
    isHex(second) &&
    !third
  );
}
/** A stored avatar as it is, or the default for one that is not an avatar. Never rewrites a valid record. */
export function normalizeAvatar(value: unknown) {
  return isAvatar(value) ? value : defaultAvatar;
}

// Earlier looks keep working: each earlier head draws as the new head (and
// accessory) nearest to it, and its colour as the nearest of the eight.
const legacyLooks: Record<string, [FaceHead, FaceAccessory]> = {
  // robot:<shape>, the previous release's heads. Its "ears" were a headband with cups.
  round: ["round", "none"], squircle: ["soft", "none"], dome: ["dome", "none"], wide: ["wide", "none"],
  antenna: ["round", "antenna"], ears: ["round", "headphones"],
  // moodstone:<cut> and bot:<shape>, which drew as the previous heads.
  circle: ["round", "none"], square: ["box", "none"], diamond: ["round", "antenna"], hexagon: ["dome", "none"],
  badge: ["round", "headphones"], burst: ["round", "antenna"], oval: ["wide", "none"], capsule: ["pill", "none"],
  triangle: ["round", "antenna"], cloud: ["cloud", "none"], drop: ["egg", "none"],
};
function toHsl(hex: string): [number, number, number] {
  const [r = 0, g = 0, b = 0] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255);
  const max = Math.max(r, g, b), min = Math.min(r, g, b), l = (max + min) / 2;
  if (max === min) return [0, 0, l];
  const d = max - min, s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
  const h = max === r ? (g - b) / d + (g < b ? 6 : 0) : max === g ? (b - r) / d + 2 : (r - g) / d + 4;
  return [h * 60, s, l];
}
/**
 * The palette colour nearest a hex: greys and near-blacks are black, any other
 * colour takes the palette hue nearest its own, so an earlier red is the pink
 * beside it and an earlier brown the orange.
 */
export function nearestAvatarColor(hex: string): AvatarColor {
  const exact = avatarColors.find((color) => color === hex.toLowerCase());
  if (exact) return exact;
  if (!isHex(hex)) return "#328cff";
  const [h, s, l] = toHsl(hex);
  if (s < 0.15 || l < 0.18) return "#18181b";
  let best: AvatarColor = "#328cff", distance = Infinity;
  for (const color of avatarColors) {
    if (color === "#18181b") continue;
    const d = Math.abs(((toHsl(color)[0] - h + 540) % 360) - 180);
    if (d < distance) (best = color), (distance = d);
  }
  return best;
}
/** The screen-face head an avatar draws as, or null for a photo or an icon. */
export function faceAvatar(value: unknown): FaceAvatar | null {
  const avatar = normalizeAvatar(value);
  const parts = avatar.split(":");
  const [prefix, shape = "", second = "", third = ""] = parts;
  if (prefix === "robot" && parts.length === 4)
    return {
      head: faceHeads.find((head) => head === shape) ?? "soft",
      accessory: faceAccessories.find((accessory) => accessory === second) ?? "none",
      color: nearestAvatarColor(third),
    };
  if (prefix === "robot" || prefix === "moodstone" || prefix === "bot") {
    const [head, accessory] = legacyLooks[shape] ?? ["round", "none"];
    return { head, accessory, color: nearestAvatarColor(second) };
  }
  return null;
}
/** The name of the palette colour an avatar wears, for its tints (`bot-<name>-tint`); undefined for a photo or an icon. */
export function avatarColorName(value: unknown): AvatarColorName | undefined {
  const face = faceAvatar(value);
  return face ? lookColors[avatarColors.indexOf(face.color)] : undefined;
}
/** The canonical value of a face avatar, so an earlier record and a new one wearing the same look compare equal. */
function faceKey(value: unknown) {
  const face = faceAvatar(value);
  return face ? faceAvatarValue(face) : String(value).toLowerCase();
}

/** Colour names the previous palette had, and the colour each is drawn as now. */
export const legacyLookColors = ["red", "brown"] as const;
const legacyColorLooks: Record<(typeof legacyLookColors)[number], AvatarColorName> = { red: "pink", brown: "orange" };
/**
 * A look, as a Bot names it, drawn as the avatar it stands for; undefined for
 * one that names nothing. A look written for the previous heads (`antenna`,
 * `ears`, `squircle`) still names its nearest head and accessory.
 */
export function lookAvatar(look: { head?: unknown; accessory?: unknown; color?: unknown } | undefined) {
  const named = includes(legacyLookColors, look?.color) ? legacyColorLooks[look.color] : look?.color;
  const color = avatarColors[lookColors.findIndex((value) => value === named)];
  if (!color) return undefined;
  const head = faceHeads.find((value) => value === look?.head);
  const legacy = !head && includes(legacyRobotShapes, look?.head) ? legacyLooks[look.head] : undefined;
  if (!head && !legacy) return undefined;
  const accessory = faceAccessories.find((value) => value === look?.accessory) ?? legacy?.[1] ?? "none";
  return faceAvatarValue({ head: head ?? legacy?.[0] ?? "soft", accessory, color });
}
/**
 * An avatar nobody in `taken` wears, starting from the one asked for: its
 * colour changes first, then its head, then its accessory, so a teammate keeps
 * as much of the look it was given as it can. The default is never handed
 * out, because a row of default heads is what made new teammates
 * indistinguishable.
 */
export function freshAvatar(taken: ReadonlySet<string>, wanted?: string) {
  const start = faceAvatar(wanted ?? defaultAvatar) ?? faceAvatar(defaultAvatar);
  const headAt = Math.max(0, faceHeads.indexOf(start?.head ?? "soft"));
  const accessoryAt = Math.max(0, faceAccessories.indexOf(start?.accessory ?? "antenna"));
  const colorAt = Math.max(0, avatarColors.indexOf(start?.color ?? "#328cff"));
  const worn = new Set([...taken].map(faceKey));
  const defaults = new Set([defaultAvatar, ...legacyDefaultAvatars].map(faceKey));
  for (let a = 0; a < faceAccessories.length; a++)
    for (let h = 0; h < faceHeads.length; h++)
      for (let c = 0; c < avatarColors.length; c++) {
        const candidate = faceAvatarValue({
          head: faceHeads[(headAt + h) % faceHeads.length] ?? "soft",
          accessory: faceAccessories[(accessoryAt + a) % faceAccessories.length] ?? "none",
          color: avatarColors[(colorAt + c) % avatarColors.length] ?? "#328cff",
        });
        if (!defaults.has(candidate) && !worn.has(candidate)) return candidate;
      }
  return wanted && isAvatar(wanted) ? wanted : defaultAvatar;
}
