import { Button as ButtonPrimitive } from "@base-ui/react/button";
import type { AnchorHTMLAttributes, ComponentType, Ref } from "react";
import {
  AnimatedIcon,
  type AnimatedIconProps,
} from "../../boardui/icons/animated-icon";
import { useIconSwap } from "../../boardui/utils/use-icon-swap";
import { cn } from "../../lib/utils";
import { buttonIconSizes, buttonVariants } from "./button";

/**
 * A round, icon-only button: `Button` at its `icon` sizes, with the quieter
 * ink a toolbar wants (shadcn/ui has no separate component for it; see
 * packages/ui/shadcn-sources.json).
 *
 *            container   icon
 *   default  36×36       20×20
 *   sm       28×28       16×16
 *
 *   ghost    clear, the icon in ink-2; hover fills and inks it
 *   outline  a surface circle with a 1px hairline, for a control that must
 *            read as one on a busy ground
 *
 * Pressing shrinks it to .94. Always pass `aria-label` (it is also what a
 * tooltip says). `animate` draws an AnimatedIcon instead of `icon`, for an
 * icon that moves with its action or state:
 *   <IconButton animate={{ name: "copy", on: copied }} aria-label=… />
 */
type IconComponent = ComponentType<{
  className?: string | undefined;
  "aria-hidden"?: boolean | "true" | "false" | undefined;
}>;

/** The icon: a lucide component, or an AnimatedIcon's name and trigger. */
export type IconButtonIcon =
  | { icon: IconComponent; animate?: undefined }
  | {
      icon?: undefined;
      animate: Omit<AnimatedIconProps, "className" | "size">;
    };

interface IconButtonOwn {
  size?: "default" | "sm";
  variant?: "outline" | "ghost";
  /** Accessible name — required since there is no visible label. */
  "aria-label": string;
}

export type IconButtonProps = Omit<
  ButtonPrimitive.Props,
  "children" | "className"
> & {
  className?: string | undefined;
} & IconButtonOwn &
  IconButtonIcon;
export type IconLinkButtonProps = Omit<
  AnchorHTMLAttributes<HTMLAnchorElement>,
  "children"
> &
  IconButtonOwn &
  IconButtonIcon & { ref?: Ref<HTMLAnchorElement> };

const press = "[--press-scale:0.94] overflow-visible";
const ghostInk =
  "text-muted-foreground hover:text-foreground aria-pressed:bg-muted aria-pressed:text-foreground";

function classesOf(
  size: "default" | "sm",
  variant: "outline" | "ghost",
  className: string | undefined,
) {
  return cn(
    buttonVariants({
      variant,
      size: size === "sm" ? "icon-sm" : "icon",
    }),
    press,
    variant === "ghost" && ghostInk,
    className,
  );
}

function ButtonIcon({
  icon: Icon,
  animate,
  className,
}: {
  icon?: IconComponent | undefined;
  animate?: Omit<AnimatedIconProps, "className" | "size"> | undefined;
  className: string;
}) {
  // An icon that changes (Copy → Check) pops in; the first never does.
  const swap = useIconSwap(Icon);
  if (animate) return <AnimatedIcon {...animate} className={className} />;
  if (!Icon) return null;
  return (
    <Icon
      key={swap}
      className={cn(className, swap && "icon-swap-in")}
      aria-hidden
    />
  );
}

export function IconButton({
  icon,
  animate,
  size = "default",
  variant = "outline",
  className,
  type = "button",
  ...props
}: IconButtonProps) {
  return (
    <ButtonPrimitive
      data-slot="icon-button"
      type={type}
      className={classesOf(size, variant, className)}
      {...props}
    >
      <ButtonIcon
        icon={icon}
        animate={animate}
        className={buttonIconSizes[size === "sm" ? "icon-sm" : "icon"]}
      />
    </ButtonPrimitive>
  );
}

/** Anchor counterpart to IconButton for external and navigational actions. */
export function IconLinkButton({
  icon,
  animate,
  size = "default",
  variant = "outline",
  className,
  ref,
  ...props
}: IconLinkButtonProps) {
  return (
    <a
      ref={ref}
      data-slot="icon-button"
      className={classesOf(size, variant, className)}
      {...props}
    >
      <ButtonIcon
        icon={icon}
        animate={animate}
        className={buttonIconSizes[size === "sm" ? "icon-sm" : "icon"]}
      />
    </a>
  );
}
