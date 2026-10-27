import { useLayoutEffect, useRef, type ComponentProps, type Ref } from "react";
import { cn } from "../../lib/utils";

/**
 * shadcn/ui Textarea (see packages/ui/shadcn-sources.json), drawn in Veynode
 * Soft: the multiline sibling of `Input`, in the same `field-shell`, with the
 * same insets so a stacked input and textarea share one edge.
 *
 *   - `rows` sets the resting height (3 lines by default)
 *   - `autoResize` grows the field a line at a time up to `maxRows`, then
 *     scrolls; the native grab handle is off then
 *   - `resize` exposes the native grab handle
 */
export type TextareaSize = "medium" | "small";
export type TextareaResize = "none" | "vertical";

export type TextareaProps = Omit<
  ComponentProps<"textarea">,
  "size" | "className" | "rows" | "ref"
> & {
  size?: TextareaSize;
  className?: string | undefined;
  /** Resting height, in lines. Also the floor when `autoResize` is on. */
  rows?: number | undefined;
  /** Grow with the content instead of scrolling at `rows`. */
  autoResize?: boolean | undefined;
  /** Ceiling for `autoResize`, in lines. Past it the field scrolls. */
  maxRows?: number | undefined;
  /** Native resize handle. Ignored (forced `none`) while `autoResize` is on. */
  resize?: TextareaResize | undefined;
  /** Class for the field shell. */
  fieldClassName?: string | undefined;
  /** Ref to the <textarea> element. */
  ref?: Ref<HTMLTextAreaElement> | undefined;
  /** Ref to the field shell wrapper. */
  groupRef?: Ref<HTMLDivElement> | undefined;
};

const shell = {
  base: "field-shell relative flex w-full flex-col rounded-md",
  // 8px shell + 4px on the control = 12px to the text (10px at `small`).
  size: { medium: "p-2", small: "px-1.5 py-2" },
} as const;

const control = cn(
  "m-0 block w-full min-w-0 border-0 bg-transparent p-0 px-1 outline-none",
  "font-sans text-body-regular text-foreground placeholder:text-ink-3",
  "disabled:cursor-not-allowed disabled:text-ink-3 disabled:placeholder:text-ink-3",
);

/** Fallback line box for `text-body-regular` (14/20) before styles resolve. */
const FALLBACK_LINE_HEIGHT = 20;

function Textarea({
  size = "medium",
  rows = 3,
  autoResize = false,
  maxRows,
  resize = "vertical",
  fieldClassName,
  className,
  ref,
  groupRef,
  onInput,
  ...props
}: TextareaProps) {
  const innerRef = useRef<HTMLTextAreaElement | null>(null);

  const attachRef = (node: HTMLTextAreaElement | null) => {
    innerRef.current = node;
    if (typeof ref === "function") ref(node);
    else if (ref)
      (ref as { current: HTMLTextAreaElement | null }).current = node;
  };

  // Measure from zero so the field shrinks back as well as grows. Reading the
  // computed line box keeps the floor and ceiling honest when a consumer
  // overrides the type scale through `className`.
  const fit = () => {
    const field = innerRef.current;
    // A non-auto-resizing textarea is sized by the browser's native resize
    // handle. Do not clear its inline height during unrelated React
    // rerenders, otherwise a user's manual resize snaps back to `rows`.
    if (!field || !autoResize) return;
    const line =
      parseFloat(window.getComputedStyle(field).lineHeight) ||
      FALLBACK_LINE_HEIGHT;
    const ceiling = maxRows ? maxRows * line : Infinity;
    field.style.height = "0px";
    const next = Math.max(field.scrollHeight, rows * line);
    field.style.height = `${Math.min(next, ceiling)}px`;
    field.style.maxHeight = maxRows ? `${ceiling}px` : "";
    field.style.overflowY = next > ceiling ? "auto" : "hidden";
  };

  // No dependency list: typing on an uncontrolled field re-renders nothing,
  // so `onInput` handles keystrokes and this catches every other reason the
  // value moved (a controlled parent, a form reset, an autofill).
  useLayoutEffect(fit);

  return (
    <div
      ref={groupRef}
      data-slot="textarea-group"
      className={cn(shell.base, shell.size[size], fieldClassName)}
    >
      <textarea
        ref={attachRef}
        data-slot="textarea"
        rows={rows}
        {...props}
        autoComplete="off"
        onInput={(event) => {
          if (autoResize) fit();
          onInput?.(event);
        }}
        className={cn(
          control,
          autoResize || resize === "none" ? "resize-none" : "resize-y",
          className,
        )}
      />
    </div>
  );
}

export { Textarea };
