import { Popover as PopoverPrimitive } from "@base-ui/react/popover";
import { MENU_POPUP } from "../../boardui/overlays/menu-styles";
import { cn } from "../../lib/utils";

/**
 * shadcn/ui Popover (Base UI flavour, see packages/ui/shadcn-sources.json),
 * drawn in Veynode Soft: the menu surface (surface fill, lg corners, the
 * `float` shadow) with the menu motion, in over 140ms from its anchor and out
 * over 100ms, fading only (`MENU_POPUP`). It holds free-form content: a form,
 * a list with a field, a panel of buttons. Rows that should look like a menu's
 * come from `menu-rows.tsx`; a list of actions is a `DropdownMenu`.
 *
 * Like the menu it is not modal and sits on the `menu` layer, over a dialog.
 * Give `PopoverContent` an `anchor` when the element that opens it is not
 * its trigger (a row's whole width, a point), and `container` to portal it into
 * a fullscreen element or a native dialog.
 */
function Popover({ ...props }: PopoverPrimitive.Root.Props) {
  return <PopoverPrimitive.Root data-slot="popover" {...props} />;
}

function PopoverTrigger({
  className,
  ...props
}: PopoverPrimitive.Trigger.Props) {
  return (
    <PopoverPrimitive.Trigger
      data-slot="popover-trigger"
      className={cn(
        "cursor-pointer outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring",
        className,
      )}
      {...props}
    />
  );
}

type PopoverContentProps = PopoverPrimitive.Popup.Props &
  Pick<
    PopoverPrimitive.Positioner.Props,
    | "align"
    | "alignOffset"
    | "side"
    | "sideOffset"
    | "anchor"
    | "collisionPadding"
  > & {
    /** Where the content is portalled; the document body by default. */
    container?: PopoverPrimitive.Portal.Props["container"];
    /** Classes on the positioning wrapper. */
    positionerClassName?: string | undefined;
  };

function PopoverContent({
  className,
  align = "start",
  alignOffset = 0,
  side = "bottom",
  sideOffset = 4,
  collisionPadding = 8,
  anchor,
  container,
  positionerClassName,
  ...props
}: PopoverContentProps) {
  return (
    <PopoverPrimitive.Portal {...(container ? { container } : {})}>
      <PopoverPrimitive.Positioner
        className={cn("isolate z-menu", positionerClassName)}
        align={align}
        alignOffset={alignOffset}
        side={side}
        sideOffset={sideOffset}
        collisionPadding={collisionPadding}
        {...(anchor === undefined ? {} : { anchor })}
      >
        <PopoverPrimitive.Popup
          data-slot="popover-content"
          className={cn(
            MENU_POPUP,
            "flex max-h-(--available-height) w-72 max-w-[calc(100vw-32px)] flex-col",
            className,
          )}
          {...props}
        />
      </PopoverPrimitive.Positioner>
    </PopoverPrimitive.Portal>
  );
}

function PopoverTitle({ className, ...props }: PopoverPrimitive.Title.Props) {
  return (
    <PopoverPrimitive.Title
      data-slot="popover-title"
      className={cn("text-body-semibold text-ink", className)}
      {...props}
    />
  );
}

function PopoverDescription({
  className,
  ...props
}: PopoverPrimitive.Description.Props) {
  return (
    <PopoverPrimitive.Description
      data-slot="popover-description"
      className={cn("text-body-2-regular text-ink-2", className)}
      {...props}
    />
  );
}

export type { PopoverContentProps };
export {
  Popover,
  PopoverTrigger,
  PopoverContent,
  PopoverTitle,
  PopoverDescription,
};
