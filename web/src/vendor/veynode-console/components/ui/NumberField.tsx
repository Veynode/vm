import { Minus, Plus } from "@veynode/ui/icons";
import { useRef } from "react";
import { useI18n } from "../../i18n/provider";

/**
 * A number with − / + steppers and its unit inside the field (Veynode Soft
 * NumberField). The input keeps the id and label a `Field` gives it; a
 * stepper greys out at its bound. The value stays the text typed, as a plain
 * number input's does, so the caller validates it the same way.
 */
export function NumberField({
  id,
  "aria-labelledby": labelledBy,
  "aria-label": ariaLabel,
  name,
  required,
  className,
  value,
  onChange,
  min,
  max,
  step = 1,
  unit,
  disabled,
}: {
  id?: string;
  "aria-labelledby"?: string;
  "aria-label"?: string | undefined;
  name?: string;
  required?: boolean;
  /** Width in its layout, e.g. `w-full` in a form grid. */
  className?: string;
  value: string;
  onChange: (value: string) => void;
  min?: number | undefined;
  max?: number | undefined;
  step?: number;
  unit?: string;
  disabled?: boolean;
}) {
  const { t } = useI18n();
  const input = useRef<HTMLInputElement>(null);
  const current = Number(value);
  const known = value.trim() !== "" && Number.isFinite(current);
  const clamp = (next: number) =>
    Math.min(max ?? Infinity, Math.max(min ?? -Infinity, next));
  const stepper =
    "grid size-7 shrink-0 place-items-center rounded-sm text-ink-2 outline-none transition-colors hover:enabled:bg-surface hover:enabled:text-ink focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus disabled:cursor-default" +
    // A disabled field is dimmed once, as a whole; only a stepper at its
    // bound dims by itself.
    (disabled ? "" : " disabled:opacity-45");
  return (
    <span
      data-number-field
      className={
        "inline-flex h-9 min-w-0 items-center gap-0.5 rounded-md bg-field px-1 focus-within:bg-surface focus-within:shadow-field-ring" +
        (disabled ? " opacity-45" : "") +
        (className ? " " + className : "")
      }
    >
      <button
        type="button"
        // The input is the control a keyboard uses (arrow keys step it);
        // the steppers are for the pointer.
        tabIndex={-1}
        className={stepper}
        aria-label={t("vmConsole.decrease")}
        disabled={disabled || (known && min !== undefined && current <= min)}
        onClick={() => onChange(String(clamp((known ? current : 0) - step)))}
      >
        <Minus size={16} aria-hidden />
      </button>
      {/* The number and its unit, centred as one between the steppers; a
          press anywhere between them goes to the number. */}
      <span
        className="flex min-w-0 flex-1 cursor-text items-baseline justify-center gap-1"
        onClick={() => input.current?.focus()}
      >
        <input
          ref={input}
          id={id}
          aria-labelledby={labelledBy}
          aria-label={ariaLabel}
          name={name}
          required={required}
          type="number"
          inputMode="numeric"
          min={min}
          max={max}
          step={step}
          value={value}
          disabled={disabled}
          onChange={(event) => onChange(event.target.value)}
          // As wide as what is typed, so the number and its unit sit together.
          style={{ width: `${Math.max(value.length, 1) + 1}ch` }}
          className="min-w-0 appearance-none bg-transparent text-center text-body-semibold text-ink tabular-nums outline-none [&::-webkit-inner-spin-button]:appearance-none [&::-webkit-outer-spin-button]:appearance-none [-moz-appearance:textfield] disabled:cursor-default"
        />
        {unit ? (
          <span className="shrink-0 text-body-2-regular text-ink-2">
            {unit}
          </span>
        ) : null}
      </span>
      <button
        type="button"
        tabIndex={-1}
        className={stepper}
        aria-label={t("vmConsole.increase")}
        disabled={disabled || (known && max !== undefined && current >= max)}
        onClick={() => onChange(String(clamp((known ? current : 0) + step)))}
      >
        <Plus size={16} aria-hidden />
      </button>
    </span>
  );
}
