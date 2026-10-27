import { useRef, useState, type ReactNode } from "react";
import { Info } from "@veynode/ui/icons";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "../components/ui/tooltip";
import { cx } from "../boardui/utils/cx";

/**
 * Where an overlay opened from `node` must portal: the open native `<dialog>`
 * or popover the node sits in, the nearest if both. Either is in the top
 * layer (a modal `<dialog>` also makes the rest of the page inert), so a
 * tooltip or popover portalled to `body` would draw behind it and never take
 * the pointer; portalled into it, it draws above its content and stays
 * usable. Anywhere else it is `undefined`, and the overlay portals to `body`
 * as before.
 */
export function topLayerContainer(
  node: Element | null | undefined,
): Element | undefined {
  return node?.closest("dialog[open], [popover]:popover-open") ?? undefined;
}

const SIDES = {
  top: "top",
  bottom: "bottom",
  start: "inline-start",
  end: "inline-end",
} as const;

/**
 * The one home for an explanation (DESIGN.md "Copy"): an ⓘ beside a label
 * that opens a tooltip of one or two short sentences on hover, a tap, or
 * keyboard focus and Enter, and closes on Escape, on leaving it, or on a
 * second tap. While open, the words describe the button (`aria-describedby`).
 *
 * `label` names the mark ("About default model"); the explanation is the
 * children. Headers never carry one.
 *
 *   <Label>{t("x.defaultModel")}</Label>
 *   <InfoTip label={t("x.aboutDefaultModel")}>{t("x.defaultModelHelp")}</InfoTip>
 */
export function InfoTip({
  label,
  children,
  placement = "top",
  className,
}: {
  label: string;
  children: ReactNode;
  placement?: "top" | "bottom" | "start" | "end";
  className?: string;
}) {
  const [open, setOpen] = useState(false);
  const mark = useRef<HTMLButtonElement>(null);
  const pointer = useRef("mouse");
  // Read once open, when the mark is mounted: inside a native modal dialog
  // (the desktop upload and clipboard panels) the tooltip portals into it.
  const container = open ? topLayerContainer(mark.current) : undefined;
  return (
    <Tooltip open={open} onOpenChange={setOpen}>
      <TooltipTrigger
        ref={mark}
        aria-label={label}
        data-info-tip=""
        data-open={open || undefined}
        onPointerDown={(event) => {
          pointer.current = event.pointerType;
        }}
        // A tap (touch has no hover) toggles it; a click or Enter keeps it
        // open (keyboard focus has already opened it).
        onClick={() => {
          setOpen(
            pointer.current === "touch" || pointer.current === "pen"
              ? !open
              : true,
          );
          pointer.current = "mouse";
        }}
        className={cx(
          "inline-grid size-[18px] shrink-0 cursor-help place-items-center rounded-full align-middle text-ink-3 outline-none transition-colors duration-[var(--button-transition-ms)]",
          "hover:text-ink data-[open]:text-ink focus-visible:text-ink",
          "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring",
          // A larger target on touch without moving the mark.
          "relative before:absolute before:-inset-[13px] before:content-[''] pointer-fine:before:hidden",
          className,
        )}
      >
        <Info className="size-4" aria-hidden />
      </TooltipTrigger>
      <TooltipContent
        side={SIDES[placement]}
        container={container as HTMLElement | undefined}
        // In a dialog it sizes to its words (`w-max`, still at most 240px),
        // not to the room left between its left edge and the dialog's.
        {...(container ? { className: "w-max" } : {})}
      >
        {children}
      </TooltipContent>
    </Tooltip>
  );
}
