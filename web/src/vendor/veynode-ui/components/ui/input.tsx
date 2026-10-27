import { Input as InputPrimitive } from "@base-ui/react/input";
import type { ComponentType, ReactNode, Ref } from "react";
import { cn } from "../../lib/utils";

/**
 * shadcn/ui Input (Base UI flavour, see packages/ui/shadcn-sources.json),
 * drawn in Veynode Soft: a `field` fill, md radius, no border. Focus turns it
 * to `surface` with the field ring (a 1px line-strong edge and a soft halo),
 * never a hard 2px ring; `aria-invalid` draws the edge in the failed colour.
 * The fill is the shell around the control (`field-shell`), so a leading or
 * trailing icon or control stands inside the same field.
 *
 *   medium  36   small  28
 */
export type InputSize = "medium" | "small";

type IconComponent = ComponentType<{
  className?: string | undefined;
  "aria-hidden"?: boolean | "true" | "false" | undefined;
}>;

export type InputProps = Omit<
  InputPrimitive.Props,
  "size" | "className" | "ref"
> & {
  size?: InputSize;
  className?: string | undefined;
  leadingIcon?: IconComponent | undefined;
  trailingIcon?: IconComponent | undefined;
  /** Custom element rendered in the leading slot. */
  leadingAddon?: ReactNode;
  /**
   * A control that lives inside the field at its end (a Save that shows once
   * there is something to save), 28px tall on the field's 4px inset.
   */
  trailingAddon?: ReactNode;
  /** Class for the field shell. */
  fieldClassName?: string | undefined;
  /** Ref to the <input> element. */
  ref?: Ref<HTMLInputElement> | undefined;
  /** Ref to the field shell wrapper. */
  groupRef?: Ref<HTMLDivElement> | undefined;
};

const shell = {
  base: "field-shell relative flex w-full items-center rounded-md",
  size: {
    medium: "min-h-9 px-2 py-2", // 36
    small: "h-7 px-1.5 py-1", // 28
  },
  withAddon: {
    medium: "h-9 py-2 pl-1 pr-2",
    small: "h-7 py-1 pl-1 pr-1.5",
  },
  // A trailing control takes the place of the right padding: the 28px button
  // on a 4px inset fills the 36px field.
  withTrailing: {
    medium: "min-h-9 py-1 pl-2 pr-1",
    small: "h-7 py-0.5 pl-1.5 pr-0.5",
  },
} as const;

const control = cn(
  "m-0 min-w-0 flex-1 truncate border-0 bg-transparent p-0 pl-1 outline-none",
  "font-sans text-body-regular text-foreground placeholder:text-ink-3",
  "disabled:cursor-not-allowed disabled:text-ink-3 disabled:placeholder:text-ink-3",
);

/**
 * A date or time field with no value draws today's date in grey in WebKit (a
 * phone's Safari too), which reads as a chosen date. While such a field is
 * empty and not being edited its native text is not drawn: it shows blank,
 * its label and hint say what blank means. Focus brings the segments back.
 */
const EMPTY_PICKER =
  "[&[data-empty-picker]:not(:focus)::-webkit-datetime-edit]:opacity-0";
const PICKER_TYPES = new Set([
  "date",
  "time",
  "datetime-local",
  "month",
  "week",
]);

function Input({
  size = "medium",
  leadingIcon: Leading,
  trailingIcon: Trailing,
  leadingAddon,
  trailingAddon,
  fieldClassName,
  className,
  ref,
  groupRef,
  ...props
}: InputProps) {
  const emptyPicker = PICKER_TYPES.has(props.type ?? "") && props.value === "";
  const hasAddon = leadingAddon !== undefined && leadingAddon !== null;
  const hasTrailing =
    trailingAddon !== undefined &&
    trailingAddon !== null &&
    trailingAddon !== false;
  return (
    <div
      ref={groupRef}
      data-slot="input-group"
      className={cn(
        shell.base,
        hasTrailing
          ? shell.withTrailing[size]
          : hasAddon
            ? shell.withAddon[size]
            : shell.size[size],
        fieldClassName,
      )}
    >
      <div className="flex w-full min-w-0 items-center gap-2">
        <div className="flex min-w-0 flex-1 items-center gap-1">
          {hasAddon ? (
            leadingAddon
          ) : Leading ? (
            <Leading className="size-4 shrink-0" aria-hidden />
          ) : null}
          <InputPrimitive
            ref={ref}
            data-slot="input"
            // Off unless the caller names what the field holds (a sign-in
            // form's `username webauthn`, `current-password`, `new-password`).
            autoComplete={props.autoComplete ?? "off"}
            {...props}
            {...(emptyPicker ? { "data-empty-picker": "" } : {})}
            className={cn(control, emptyPicker && EMPTY_PICKER, className)}
          />
        </div>
        {Trailing ? <Trailing className="size-4 shrink-0" aria-hidden /> : null}
        {hasTrailing ? trailingAddon : null}
      </div>
    </div>
  );
}

export { Input };
