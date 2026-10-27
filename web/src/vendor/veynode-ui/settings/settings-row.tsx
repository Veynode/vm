import {
  useId,
  type FormHTMLAttributes,
  type ReactElement,
  type ReactNode,
} from "react";
import type { LucideIcon } from "@veynode/ui/icons";
import { Label } from "../components/ui/label";
import { cx } from "../boardui/utils/cx";
import { associateControl } from "./field";
import { InfoTip } from "./info-tip";
import { RowIcon } from "./list-row";
import { useStateFit } from "./state-fit";

/**
 * An info mark whose tooltip holds words that would otherwise sit under a
 * label or a title; `label` names the mark (the words themselves when they
 * are plain text). The same as `InfoTip`, kept under its older name.
 */
export function InfoHint({
  label,
  children,
  className,
}: {
  label: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <InfoTip label={label} {...(className === undefined ? {} : { className })}>
      {children}
    </InfoTip>
  );
}

/**
 * Label on the left, then at the row's end its state, what it is set to
 * (`value`) and one control, one line of 48px at every width, a phone
 * included. Short of room a long value is cut first, then the name; the
 * state keeps its natural width and is a dot only where even the name at its
 * minimum does not fit beside it (`useStateFit`); nothing moves under the
 * label. The control receives the label
 * association; `actions` do not. Callers size the control (for example
 * `className="w-60"`); it shrinks with the row. A `hint` is kept behind an
 * info icon beside the label rather than under it; a hint that is not plain
 * text says in `hintLabel` what the icon is named. An `icon` leads the row,
 * 20px in the secondary ink, with no plate.
 */
/** A row's label reads as the row's name: body, 500, ink. */
const ROW_LABEL = "text-body-medium";

export function SettingsRow({
  icon: Icon,
  label,
  state,
  value,
  hint,
  hintLabel,
  control,
  actions,
  as: Root = "div",
  className,
  ...rest
}: {
  icon?: LucideIcon;
  label: ReactNode;
  /** Its state, as a `StateTag` before what it is set to; it stays whole while the name is cut. */
  state?: ReactNode;
  /** What the row is set to now, in a few words; cut when long, a path too. */
  value?: ReactNode;
  control?: ReactElement;
  actions?: ReactNode;
  as?: "div" | "form";
  className?: string;
} & (
  | { hint?: string; hintLabel?: undefined }
  | { hint?: ReactNode; hintLabel: string }
) &
  Omit<FormHTMLAttributes<HTMLElement>, "title">) {
  const id = useId();
  const root = useStateFit();
  return (
    <Root
      {...rest}
      ref={root}
      className={cx(
        "group/setting flex min-h-12 items-center justify-between gap-x-4 border-b border-line py-2 first:pt-0 last:border-b-0 last:pb-0",
        className,
      )}
    >
      {/* The name is cut after the state, never moved. */}
      <div
        className={cx(
          "min-w-0 flex-[1_1_auto]",
          Icon ? "flex items-center gap-3" : "space-y-0.5",
        )}
      >
        {Icon && <RowIcon icon={Icon} />}
        {hint ? (
          <div className="flex min-w-0 items-center gap-1">
            <Label
              id={`${id}-label`}
              htmlFor={id}
              className={cx(ROW_LABEL, "min-w-0")}
            >
              <span data-row-name="" className="min-w-0 truncate">
                {label}
              </span>
            </Label>
            <InfoHint label={hintLabel ?? String(hint)}>{hint}</InfoHint>
          </div>
        ) : (
          <Label
            id={`${id}-label`}
            htmlFor={id}
            className={cx(ROW_LABEL, "min-w-0")}
          >
            <span data-row-name="" className="min-w-0 truncate">
              {label}
            </span>
          </Label>
        )}
      </div>
      {/* The control and the actions sit at the row's end, centred on the
          row and never stretched to its height; a button keeps its width,
          a field or a select shrinks with the row. */}
      {(state || value || control || actions) && (
        <div
          className={cx(
            "flex min-w-0 items-center justify-end gap-3 has-[:is(input:not([aria-hidden=true]),textarea,[data-select-trigger])]:min-w-28 [&>:is(div,input,textarea)]:min-w-28 [&>:is(button,a)]:shrink-0",
            // A state keeps its width: the end of the row stands at no more
            // than 70% and the name gives way to it.
            state ? "max-w-[70%] shrink-0" : "shrink-[100000]",
          )}
        >
          {state && (
            <span
              data-row-status=""
              className="group/state flex shrink-0 items-center"
            >
              {state}
            </span>
          )}
          {value && (
            <span
              data-row-value=""
              className="min-w-0 truncate text-body-2-regular text-ink-2 tabular-nums"
            >
              {value}
            </span>
          )}
          {control && associateControl(control, id)}
          {actions}
        </div>
      )}
    </Root>
  );
}
