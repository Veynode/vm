import { createLucideIcon, type IconNode } from "lucide-react";
import type { ComponentProps } from "react";
import {
  BUBBLE,
  COG,
  COG_HOLE,
  DRAWINGS,
  EYE_HOLES,
  SHAPES_CIRCLE,
  SHAPES_SQUARE,
  SHAPES_TRIANGLE,
} from "./drawings";

/**
 * Every icon the app draws (ds 「图标」). lucide's set, with Veynode's own
 * drawing wherever one exists: a name exported below wins over lucide's, so
 * redrawing an icon changes no call site. Import icons from here, never from
 * lucide-react (`pnpm check:design`).
 */
export * from "lucide-react";

const icon = (name: keyof typeof DRAWINGS) =>
  createLucideIcon(
    name,
    DRAWINGS[name].map(([tag, attributes], index) => [
      tag,
      { ...attributes, key: String(index) },
    ]) as IconNode,
  );

export const MessageCircle = icon("message-circle");
export const Settings = icon("settings");
export const Server = icon("server");
export const Bot = icon("bot");
export const Monitor = icon("monitor");
export const Cpu = icon("cpu");
export const MessagesSquare = icon("messages-square");
export const Folder = icon("folder");
export const Shapes = icon("shapes");

type SolidProps = Omit<ComponentProps<"svg">, "children"> & {
  size?: number | string;
};
/** A place's icon while it is the one shown: the same outline, filled, its marks cut out. */
const solid = (d: string) =>
  function SolidIcon({ size = 24, ...props }: SolidProps) {
    return (
      <svg
        width={size}
        height={size}
        viewBox="0 0 24 24"
        aria-hidden
        {...props}
      >
        <path d={d} fill="currentColor" fillRule="evenodd" />
      </svg>
    );
  };
export const MessageCircleSolid = solid(`${BUBBLE} ${EYE_HOLES}`);
export const SettingsSolid = solid(`${COG} ${COG_HOLE}`);
export const ShapesSolid = solid(
  `${SHAPES_TRIANGLE} ${SHAPES_SQUARE} ${SHAPES_CIRCLE}`,
);
