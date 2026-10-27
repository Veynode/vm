import { isSticker } from "../ui/Sticker";
import {
  avatarColors,
  avatarIcons,
  isAvatar,
  isDefaultAvatar,
  lookColors,
  nearestAvatarColor,
  type AvatarColorName,
} from "@veynode/shared/avatar";

/**
 * A computer's icon (ds 电脑图标, 贴纸): what is stored for it, and which
 * drawing and state a computer shows. The drawings are in
 * `computer-stickers.tsx`; `ComputerIcon.tsx` draws them.
 */

/** The glyphs a person can choose (`icon:<kind>:<hex>`): every one drawn. */
export type StoredComputerKind = (typeof avatarIcons)[number];
/** Every glyph, chosen or the automatic icon's. */
export type ComputerIconKind = StoredComputerKind;
/** A colour of the picker: no colour (neutral, "Auto") or one of the eight. */
export type ComputerIconColor = "neutral" | AvatarColorName;
export const computerIconColors: readonly ComputerIconColor[] = [
  "neutral",
  ...lookColors,
];
export type ComputerIconState = "idle" | "working" | "off";
export type ComputerIconTheme = "light" | "dark";

// ---------- the stored value ----------

/**
 * What every computer is given when it is made, and what Reset writes: no
 * icon chosen, so the kind of machine shows. The server and older clients
 * write it, so only this exact value reads as not chosen.
 */
export const UNCHOSEN_COMPUTER_ICON = "icon:monitor:#18181b";
/**
 * The colour a chosen glyph stores for "Auto" (no colour). The format needs a
 * hex; this grey is the one earlier releases already drew as neutral ink.
 */
export const NEUTRAL_ICON_HEX = "#929292";
/**
 * A chosen black monitor. Lowercase it is the unchosen value, so it is stored
 * with an uppercase hex, which every reader accepts as the same colour.
 */
const BLACK_MONITOR = "icon:monitor:#18181B";

export type ComputerLook =
  /** An uploaded picture. */
  | { type: "picture"; value: string }
  /** A Bot face someone gave the computer earlier, drawn as it was. */
  | { type: "face"; value: string }
  /** A glyph and colour; `kind` null when none was chosen (the kind of machine shows). */
  | { type: "icon"; kind: StoredComputerKind | null; color: ComputerIconColor };

const isStoredKind = (value: unknown): value is StoredComputerKind =>
  avatarIcons.some((kind) => kind === value);

/** The palette colour a stored hex stands for. */
export function iconColorOfHex(hex: string): ComputerIconColor {
  const lower = hex.toLowerCase();
  if (lower === NEUTRAL_ICON_HEX) return "neutral";
  const color = nearestAvatarColor(lower);
  return lookColors[avatarColors.indexOf(color)] ?? "neutral";
}

/** What a computer's stored `avatar` draws as. */
export function computerLook(value: unknown): ComputerLook {
  const unchosen: ComputerLook = { type: "icon", kind: null, color: "neutral" };
  if (
    !isAvatar(value) ||
    isDefaultAvatar(value) ||
    value === UNCHOSEN_COMPUTER_ICON
  )
    return unchosen;
  if (value.startsWith("data:")) return { type: "picture", value };
  const [prefix, kind, hex = ""] = value.split(":");
  if (prefix !== "icon") return { type: "face", value };
  return isStoredKind(kind)
    ? { type: "icon", kind, color: iconColorOfHex(hex) }
    : unchosen;
}

/** Whether a stored avatar leaves the computer's icon to its kind. */
export const isUnchosenComputerIcon = (value: unknown) => {
  const look = computerLook(value);
  return look.type === "icon" && look.kind === null && look.color === "neutral";
};

/**
 * The value a choice stores. No glyph and no colour is not chosen; a colour
 * without a glyph needs one, so it takes `fallback` (the kind shown, when it
 * can be stored).
 */
export function computerIconValue(
  kind: StoredComputerKind | null,
  color: ComputerIconColor,
  fallback: ComputerIconKind = "monitor",
): string {
  if (!kind && color === "neutral") return UNCHOSEN_COMPUTER_ICON;
  const glyph = kind ?? (isStoredKind(fallback) ? fallback : "monitor");
  if (color === "neutral") return `icon:${glyph}:${NEUTRAL_ICON_HEX}`;
  if (glyph === "monitor" && color === "black") return BLACK_MONITOR;
  return `icon:${glyph}:${avatarColors[lookColors.indexOf(color)] ?? NEUTRAL_ICON_HEX}`;
}

// ---------- which glyph and state a computer shows ----------

/**
 * The facts of a computer record the icon reads. Any record carries them (a
 * typed `Computer`, an editor's loose item, a host's report of a VM), so
 * each is read for what it is rather than trusted to be typed.
 */
export type ComputerFacts = {
  readonly avatar?: unknown;
  readonly status?: unknown;
  readonly mode?: unknown;
  readonly transport?: unknown;
  readonly managedByHost?: unknown;
  readonly environmentState?: unknown;
  readonly environmentKind?: unknown;
  readonly operatingSystem?: unknown;
  readonly headless?: unknown;
  readonly [key: string]: unknown;
};

/**
 * What a computer shows when it has no icon of its own: the kind of machine,
 * so a Mac VM, a Linux container, a command-line computer and a device
 * someone connected do not all read as the same monitor.
 */
export function kindIcon(computer: ComputerFacts): ComputerIconKind {
  if (computer.operatingSystem === "macos")
    return computer.environmentKind === "virtual-machine" ? "vm" : "monitor";
  // A computer without a desktop is its command line, whatever runs it.
  if (computer.headless) return "terminal";
  if (
    computer.environmentKind === "container" ||
    (computer.transport !== "remote" && !computer.managedByHost)
  )
    return "box";
  if (computer.transport === "remote" && !computer.managedByHost)
    return "laptop";
  return "monitor";
}

/**
 * The icon's state, read as the computer's status badge reads it: working
 * while a Bot holds it (`mode: "bot"`), off while it is stopped, failed to
 * start or cannot be reached, idle otherwise.
 */
export function computerIconState(computer: ComputerFacts): ComputerIconState {
  const status = String(
    (computer.managedByHost ? computer.environmentState : undefined) ??
      computer.status ??
      "",
  );
  const device = computer.transport === "remote" && !computer.managedByHost;
  if (["stopped", "offline", "error", "failed", "deleted"].includes(status))
    return "off";
  if (device && status !== "running" && status !== "starting") return "off";
  return status === "running" && computer.mode === "bot" ? "working" : "idle";
}

// ---------- the sticker (ds 电脑图标, 贴纸) ----------

/** While a Bot works: a glow in the icon's colour, as a CSS filter. */
export const workingGlow = (size: number, color: string) =>
  `drop-shadow(0 0 ${!isSticker(size) ? 1.5 : size <= 40 ? 2.5 : 4}px ${color})`;
