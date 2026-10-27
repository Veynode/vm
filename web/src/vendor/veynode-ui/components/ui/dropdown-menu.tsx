import { Menu as MenuPrimitive } from "@base-ui/react/menu";
import {
  MENU_HEADING,
  MENU_POPUP,
  MENU_SEPARATOR,
} from "../../boardui/overlays/menu-styles";
import { Check, ChevronRight } from "../../icons";
import { cn } from "../../lib/utils";
import { menuRowStyles } from "./menu-rows";

/**
 * shadcn/ui Dropdown Menu (Base UI flavour, see packages/ui/shadcn-sources.json),
 * drawn in Veynode Soft: a surface panel with lg corners and the `float`
 * shadow, 6px of padding and 2px between rows (36px, 44px on a phone's touch
 * screen; see `menuRowStyles`). It opens in 140ms from its anchor, up 4px and
 * from .97, and leaves in 100ms, fading only (`MENU_POPUP`). The row the
 * pointer or the keyboard is at fills surface-2 (`data-highlighted`); a
 * checked radio or checkbox row fills it too and carries a check at its end.
 *
 * Two differences from shadcn: the menu is not modal (opening one locks no
 * scroll, which would yank sticky layout), and the content sits on the
 * `menu` layer, over a dialog (`layer="top"` for a context menu). A context
 * menu is positioned `fixed` and does not track its anchor: the positioner's
 * resize observer otherwise writes layout in the same turn as the
 * transcript's, and WebKit reports that loop.
 *
 * Anchor it to a trigger (`<DropdownMenuTrigger render={<Button .../>} />`)
 * or, for a context menu at a point, give `DropdownMenuContent` an `anchor`.
 */
function DropdownMenu({ modal = false, ...props }: MenuPrimitive.Root.Props) {
  return (
    <MenuPrimitive.Root data-slot="dropdown-menu" modal={modal} {...props} />
  );
}

function DropdownMenuPortal({ ...props }: MenuPrimitive.Portal.Props) {
  return <MenuPrimitive.Portal data-slot="dropdown-menu-portal" {...props} />;
}

function DropdownMenuTrigger({
  className,
  ...props
}: MenuPrimitive.Trigger.Props) {
  return (
    <MenuPrimitive.Trigger
      data-slot="dropdown-menu-trigger"
      className={cn(
        "cursor-pointer outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring",
        className,
      )}
      {...props}
    />
  );
}

type DropdownMenuContentProps = MenuPrimitive.Popup.Props &
  Pick<
    MenuPrimitive.Positioner.Props,
    | "align"
    | "alignOffset"
    | "side"
    | "sideOffset"
    | "anchor"
    | "collisionPadding"
  > & {
    /** `top` puts a context menu over everything, a viewer included. */
    layer?: "menu" | "top";
    /** Where the content is portalled; the document body by default. */
    container?: MenuPrimitive.Portal.Props["container"];
    /** Classes on the positioning wrapper, for a width the content cannot set. */
    positionerClassName?: string | undefined;
  };

function DropdownMenuContent({
  align = "start",
  alignOffset = 0,
  side = "bottom",
  sideOffset = 4,
  collisionPadding = 8,
  anchor,
  layer = "menu",
  container,
  positionerClassName,
  className,
  ...props
}: DropdownMenuContentProps) {
  return (
    <MenuPrimitive.Portal {...(container ? { container } : {})}>
      <MenuPrimitive.Positioner
        className={cn(
          "isolate outline-none",
          layer === "top" ? "z-top" : "z-menu",
          positionerClassName,
        )}
        {...(layer === "top"
          ? { positionMethod: "fixed" as const, disableAnchorTracking: true }
          : {})}
        align={align}
        alignOffset={alignOffset}
        side={side}
        sideOffset={sideOffset}
        collisionPadding={collisionPadding}
        {...(anchor === undefined ? {} : { anchor })}
      >
        <MenuPrimitive.Popup
          data-slot="dropdown-menu-content"
          className={cn(
            MENU_POPUP,
            "flex max-h-(--available-height) w-max max-w-[min(260px,calc(100vw-32px))] min-w-[200px] flex-col gap-0.5 overflow-y-auto p-1.5",
            "data-ending-style:overflow-hidden",
            className,
          )}
          {...props}
        />
      </MenuPrimitive.Positioner>
    </MenuPrimitive.Portal>
  );
}

function DropdownMenuGroup({ ...props }: MenuPrimitive.Group.Props) {
  return <MenuPrimitive.Group data-slot="dropdown-menu-group" {...props} />;
}

/** A small grey heading over a group of rows. */
function DropdownMenuLabel({
  className,
  ...props
}: MenuPrimitive.GroupLabel.Props) {
  return (
    <MenuPrimitive.GroupLabel
      data-slot="dropdown-menu-label"
      className={cn(MENU_HEADING, className)}
      {...props}
    />
  );
}

const itemStyles = cn(menuRowStyles, "data-highlighted:bg-surface-2");

function DropdownMenuItem({
  className,
  variant = "default",
  ...props
}: MenuPrimitive.Item.Props & {
  /** `destructive`: the row's words and icon in the failed ink. */
  variant?: "default" | "destructive";
}) {
  return (
    <MenuPrimitive.Item
      data-slot="dropdown-menu-item"
      data-variant={variant}
      className={cn(itemStyles, className)}
      {...props}
    />
  );
}

function DropdownMenuSub({ ...props }: MenuPrimitive.SubmenuRoot.Props) {
  return <MenuPrimitive.SubmenuRoot data-slot="dropdown-menu-sub" {...props} />;
}

function DropdownMenuSubTrigger({
  className,
  children,
  ...props
}: MenuPrimitive.SubmenuTrigger.Props) {
  return (
    <MenuPrimitive.SubmenuTrigger
      data-slot="dropdown-menu-sub-trigger"
      className={cn(itemStyles, "data-popup-open:bg-surface-2", className)}
      {...props}
    >
      {children}
      <ChevronRight className="ml-auto" aria-hidden />
    </MenuPrimitive.SubmenuTrigger>
  );
}

function DropdownMenuSubContent({
  align = "start",
  alignOffset = -6,
  side = "right",
  sideOffset = 0,
  ...props
}: DropdownMenuContentProps) {
  return (
    <DropdownMenuContent
      data-slot="dropdown-menu-sub-content"
      align={align}
      alignOffset={alignOffset}
      side={side}
      sideOffset={sideOffset}
      {...props}
    />
  );
}

/** A chosen row fills like the hovered one. */
const checked = "data-checked:bg-surface-2";

function DropdownMenuCheckboxItem({
  className,
  children,
  ...props
}: MenuPrimitive.CheckboxItem.Props) {
  return (
    <MenuPrimitive.CheckboxItem
      data-slot="dropdown-menu-checkbox-item"
      className={cn(itemStyles, checked, className)}
      {...props}
    >
      {children}
      <MenuPrimitive.CheckboxItemIndicator
        data-slot="dropdown-menu-checkbox-item-indicator"
        className="ml-auto flex shrink-0 items-center"
      >
        <Check className="size-4 text-ink-2" aria-hidden />
      </MenuPrimitive.CheckboxItemIndicator>
    </MenuPrimitive.CheckboxItem>
  );
}

function DropdownMenuRadioGroup({ ...props }: MenuPrimitive.RadioGroup.Props) {
  return (
    <MenuPrimitive.RadioGroup
      data-slot="dropdown-menu-radio-group"
      {...props}
    />
  );
}

function DropdownMenuRadioItem({
  className,
  children,
  ...props
}: MenuPrimitive.RadioItem.Props) {
  return (
    <MenuPrimitive.RadioItem
      data-slot="dropdown-menu-radio-item"
      className={cn(itemStyles, checked, className)}
      {...props}
    >
      {children}
      <MenuPrimitive.RadioItemIndicator
        data-slot="dropdown-menu-radio-item-indicator"
        className="ml-auto flex shrink-0 items-center"
      >
        <Check className="size-4 text-ink-2" aria-hidden />
      </MenuPrimitive.RadioItemIndicator>
    </MenuPrimitive.RadioItem>
  );
}

/** Inset divider between groups of rows. */
function DropdownMenuSeparator({
  className,
  ...props
}: MenuPrimitive.Separator.Props) {
  return (
    <MenuPrimitive.Separator
      data-slot="dropdown-menu-separator"
      className={cn(MENU_SEPARATOR, className)}
      {...props}
    />
  );
}

export type { DropdownMenuContentProps };
export {
  DropdownMenu,
  DropdownMenuPortal,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuGroup,
  DropdownMenuLabel,
  DropdownMenuItem,
  DropdownMenuCheckboxItem,
  DropdownMenuRadioGroup,
  DropdownMenuRadioItem,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubTrigger,
  DropdownMenuSubContent,
};
