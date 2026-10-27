import { Button as ButtonPrimitive } from "@base-ui/react/button";
import { X } from "../../icons";
import { cn } from "../../lib/utils";
import { buttonVariants } from "./button";

/**
 * Compact circular dismiss control — the "X" used to close announcements,
 * toasts, modals, and drawers. Ships its own glyph (rather than taking an
 * `icon` prop) so every consumer gets an identical X: the app's one icon
 * style (1.5 stroke, round caps), sized to the container. The glyph never
 * moves: closing is not an action to celebrate, and the button's own press
 * is the only feedback (`icon-still` keeps the glyph out of the pressed
 * icon's scale).
 *
 * Sizes (px):
 *   2xs = 16 container, 12 glyph   (compact floating surfaces)
 *   xs  = 20 container, 14 glyph   (Announcement dismiss corner)
 *   sm  = 28 container, 16 glyph   (dialog header close, a small IconButton)
 *   md  = 36 container, 20 glyph   (a medium IconButton)
 */
type CloseButtonSize = "2xs" | "xs" | "sm" | "md";

const GLYPH: Record<CloseButtonSize, string> = {
  "2xs": "size-3",
  xs: "size-3.5",
  sm: "size-4",
  md: "size-5",
};

const CONTAINER: Record<CloseButtonSize, string> = {
  "2xs": "size-4",
  xs: "size-5",
  sm: "size-7",
  md: "size-9",
};

export type CloseButtonProps = Omit<
  ButtonPrimitive.Props,
  "children" | "className"
> & {
  className?: string | undefined;
  size?: CloseButtonSize;
  /** Accessible name — required since there is no visible label. */
  "aria-label": string;
};

export function CloseButton({
  size = "xs",
  className,
  type = "button",
  ...props
}: CloseButtonProps) {
  return (
    <ButtonPrimitive
      data-slot="close-button"
      type={type}
      className={cn(
        buttonVariants({ variant: "ghost", size: "icon-xs" }),
        "p-0 text-muted-foreground hover:text-foreground [--press-scale:0.94]",
        CONTAINER[size],
        className,
      )}
      {...props}
    >
      <X className={cn("icon-still shrink-0", GLYPH[size])} aria-hidden />
    </ButtonPrimitive>
  );
}
