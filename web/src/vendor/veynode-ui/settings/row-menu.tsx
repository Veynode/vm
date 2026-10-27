import { Ellipsis, type LucideIcon } from "@veynode/ui/icons";
import { Fragment, useRef, useState } from "react";
import { Button } from "../components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "../components/ui/dropdown-menu";
import { IconButton } from "../components/ui/icon-button";

export type RowMenuItem = {
  label: string;
  onSelect: () => void;
  /** The item's icon: menus read as icon + verb. */
  icon?: LucideIcon | undefined;
  /** Destructive: in the failed ink, set apart; put it last. */
  danger?: boolean | undefined;
  /** Starts a group of its own, as a destructive action does. */
  apart?: boolean | undefined;
  disabled?: boolean | undefined;
};

/**
 * What opens the menu when it is not the ··· icon: a labelled button, such
 * as the one 「添加」 of a page whose several ways to add something are the
 * menu's items (UX principle 1). `primary` draws it as the page's one
 * primary action; otherwise it is a quiet secondary pill.
 */
export type RowMenuTrigger = {
  label: string;
  icon?: LucideIcon | undefined;
  primary?: boolean | undefined;
  /** `small` (28px) inside a row; a page's own trigger keeps `medium`. */
  size?: "small" | "medium" | undefined;
};

/**
 * The ··· menu at the end of a Settings row or page title: the actions a
 * person takes now and then, so the row itself keeps only its main one.
 * Items that do not apply are left out by the caller; with none left the
 * menu is not drawn. Choosing an item puts focus back on the trigger first,
 * so a dialog the item opens returns there when it closes. With `trigger`
 * the same menu hangs from a labelled button instead of the ···. A right
 * click on its row opens the ··· menu too (`openRowMenu`).
 */
export function RowMenu({
  label,
  items,
  disabled,
  trigger,
  placement = "bottom end",
}: {
  /**
   * Names the menu, and the ··· button, for assistive technology; a labelled
   * `trigger` is named by its own words.
   */
  label: string;
  items: readonly (RowMenuItem | false | undefined | null)[];
  disabled?: boolean;
  trigger?: RowMenuTrigger;
  placement?: "bottom end" | "bottom start";
}) {
  const [open, setOpen] = useState(false);
  const box = useRef<HTMLSpanElement>(null);
  const shown = items.filter((item): item is RowMenuItem => !!item);
  if (!shown.length) return null;
  const [side, align] = placement.split(" ") as ["bottom", "start" | "end"];
  return (
    <span
      ref={box}
      className="contents"
      {...(trigger ? {} : { "data-row-menu": "" })}
    >
      <DropdownMenu open={open} onOpenChange={setOpen}>
        {trigger ? (
          // The Button's own look (the page's primary action, or a quiet
          // outline), with the chevron that says it opens a menu.
          <DropdownMenuTrigger
            disabled={disabled === true}
            render={
              <Button
                variant={trigger.primary ? "default" : "outline"}
                size={trigger.size === "small" ? "sm" : "default"}
                leadingIcon={trigger.icon}
                trailingAnimate={{ name: "chevron-down", on: open }}
                className="pr-3"
              />
            }
          >
            {trigger.label}
          </DropdownMenuTrigger>
        ) : (
          // 28px to see, 44px to touch.
          <DropdownMenuTrigger
            disabled={disabled === true}
            render={
              <IconButton
                variant="ghost"
                size="sm"
                icon={Ellipsis}
                aria-label={label}
              />
            }
          />
        )}
        <DropdownMenuContent aria-label={label} side={side} align={align}>
          {shown.map((item, index) => {
            const Icon = item.icon;
            return (
              <Fragment key={item.label}>
                {(item.apart || item.danger) &&
                  index > 0 &&
                  !shown[index - 1]?.danger && <DropdownMenuSeparator />}
                <DropdownMenuItem
                  disabled={item.disabled === true}
                  variant={item.danger ? "destructive" : "default"}
                  onClick={() => {
                    // The menu returns focus to the trigger as it closes; the
                    // item runs once it has, so a dialog it opens returns there.
                    requestAnimationFrame(() => {
                      box.current?.querySelector("button")?.focus();
                      item.onSelect();
                    });
                  }}
                >
                  {Icon && <Icon aria-hidden />}
                  {item.label}
                </DropdownMenuItem>
              </Fragment>
            );
          })}
        </DropdownMenuContent>
      </DropdownMenu>
    </span>
  );
}
