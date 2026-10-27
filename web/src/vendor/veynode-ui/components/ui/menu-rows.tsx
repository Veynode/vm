import type { ComponentProps, ReactNode } from "react";
import {
  MENU_HEADING,
  MENU_SEPARATOR,
} from "../../boardui/overlays/menu-styles";
import { cn } from "../../lib/utils";

/**
 * The Veynode Soft menu row, shared by the menu items of dropdown-menu.tsx and
 * by the rows of a panel that looks like a menu but is not one (a time list in
 * a popover, a device list in the computer dock). Not part of shadcn/ui.
 *
 * A row is 36px (44px on a phone's touch screen) with sm corners, a 16px icon
 * 12px from its label, and fills surface-2 when it is the place the pointer
 * or the keyboard is at (`data-highlighted` on a menu item, hover and focus on
 * a plain row) or the current choice.
 */
export const menuRowStyles = [
  "flex w-full min-h-9 max-narrow:pointer-coarse:min-h-11 cursor-pointer select-none items-center justify-start gap-3 rounded-sm px-3 py-1.5 text-left text-body-regular text-ink outline-none",
  "transition-colors duration-[var(--button-transition-ms)]",
  "[&>svg]:size-4 [&>svg]:shrink-0 [&>svg]:text-ink-2",
  "data-disabled:cursor-default data-disabled:opacity-45 data-disabled:pointer-events-none",
  "data-[variant=destructive]:text-state-failed-ink data-[variant=destructive]:[&>svg]:text-state-failed-ink",
].join(" ");

type MenuRowProps = Omit<ComponentProps<"button">, "type"> & {
  /** The current choice: drawn like the hovered row. */
  selected?: boolean;
  /** A destructive row: its words and icon in the failed ink. */
  danger?: boolean;
};

/** A button row for a panel that is not a menu. */
function MenuRow({
  selected,
  danger,
  className,
  disabled,
  ...props
}: MenuRowProps) {
  return (
    <button
      type="button"
      data-slot="menu-row"
      data-variant={danger ? "destructive" : undefined}
      aria-pressed={selected}
      disabled={disabled}
      className={cn(
        menuRowStyles,
        selected
          ? "bg-surface-2"
          : "hover:bg-surface-2 focus-visible:bg-surface-2",
        "disabled:cursor-default disabled:opacity-45 disabled:hover:bg-transparent",
        className,
      )}
      {...props}
    />
  );
}

/** A small grey heading over a group of rows. */
function MenuGroup({
  label,
  className,
  children,
}: {
  label?: string | undefined;
  className?: string | undefined;
  children: ReactNode;
}) {
  return (
    <div
      data-slot="menu-group"
      role="group"
      aria-label={label}
      className={cn("flex w-full flex-col", className)}
    >
      {label && <span className={MENU_HEADING}>{label}</span>}
      <div className="flex w-full flex-col gap-0.5">{children}</div>
    </div>
  );
}

/** Inset divider between groups of rows. */
function MenuDivider({ className }: { className?: string | undefined }) {
  return (
    <div
      role="separator"
      data-slot="menu-divider"
      className={cn(MENU_SEPARATOR, className)}
    />
  );
}

export { MenuRow, MenuGroup, MenuDivider };
