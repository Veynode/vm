import type { HTMLAttributes, ReactNode } from "react";
import { cx } from "../boardui/utils/cx";

/**
 * A row that is one action, its words in the row's place (design-system/
 * settings.md, "Rows"): the way a destructive action is offered, in red, in
 * the last group of a page, where it asks first through a `ConfirmDialog`.
 * Put it straight inside a `SettingsGroup`.
 */
export function SettingsActionRow({
  label,
  tone = "danger",
  onPress,
  disabled = false,
  className,
  ...rest
}: {
  label: ReactNode;
  /** `danger` for what destroys or removes; `default` for anything else. */
  tone?: "danger" | "default";
  onPress: () => void;
  disabled?: boolean | undefined;
  className?: string;
} & Omit<HTMLAttributes<HTMLDivElement>, "title" | "onClick">) {
  return (
    <div
      data-settings-action-row=""
      className={cx(
        "flex min-h-12 items-center transition-colors duration-[var(--motion-fast)] has-[button:hover:not(:disabled)]:bg-ink/5",
        className,
      )}
      {...rest}
    >
      <button
        type="button"
        disabled={disabled}
        onClick={onPress}
        className={cx(
          "flex min-h-9 w-full min-w-0 cursor-pointer items-center text-left text-body-medium outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus disabled:cursor-default disabled:opacity-45",
          tone === "danger" ? "text-state-failed-ink" : "text-ink",
        )}
      >
        <span className="min-w-0 truncate">{label}</span>
      </button>
    </div>
  );
}
