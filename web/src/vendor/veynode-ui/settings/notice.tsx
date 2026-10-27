import type { ReactNode } from "react";
import type { LucideIcon } from "@veynode/ui/icons";
import { cx } from "../boardui/utils/cx";
import {
  AnimatedIcon,
  type AnimatedIconProps,
} from "../boardui/icons/animated-icon";
import { CloseButton } from "../components/ui/close-button";

const tones = {
  info: { box: "bg-surface-2 text-ink", icon: "text-ink-2" },
  error: {
    box: "bg-state-failed-tint text-state-failed-ink",
    icon: "text-state-failed",
  },
  done: {
    box: "bg-state-done-tint text-state-done-ink",
    icon: "text-state-done",
  },
  waiting: {
    box: "bg-state-waiting-tint text-state-waiting-ink",
    icon: "text-state-waiting-ink",
  },
} as const;

/**
 * Static inline notice (Veynode Soft): a flat box on the state's tint (or
 * surface-2 for a plain hint), md radius, the icon and words in the state's
 * ink, any action at the end. One line saying what is true; transient
 * feedback uses ToastProvider and the Modal `error` slot uses BoardUI
 * Notification. This box never animates and casts no shadow; only its
 * icon may, with `animate` in place of `icon`, when the notice's own action
 * happens (ds 「图标动效」, e.g. 开机 `power` on its 开启).
 */
export function Notice({
  status = "info",
  icon: Icon,
  animate,
  title,
  children,
  actions,
  dismiss,
  role,
  className,
}: {
  status?: "info" | "error" | "done" | "waiting";
  icon?: LucideIcon;
  animate?: Omit<AnimatedIconProps, "className" | "size"> | undefined;
  title?: ReactNode;
  children?: ReactNode;
  actions?: ReactNode;
  /** A note the person may close, with the close button's name. */
  dismiss?: { label: string; run: () => void } | undefined;
  role?: "status" | "alert";
  className?: string;
}) {
  const error = status === "error";
  const tone = tones[status];
  return (
    <div
      role={role ?? (error ? "alert" : undefined)}
      data-notice={status}
      className={cx(
        "flex items-start gap-2 rounded-md px-3 py-2",
        tone.box,
        className,
      )}
    >
      {/* The icon keeps to the words' first line, and the two are centred
          together against a taller action. */}
      <div className="flex min-w-0 flex-1 items-start gap-2 self-center">
        {(animate || Icon) && (
          // One line of the words tall, so the icon centres on that line.
          <span
            className={cx(
              "flex h-[1lh] shrink-0 items-center",
              title ? "text-body-semibold" : "text-body-medium",
            )}
          >
            {animate ? (
              <AnimatedIcon
                {...animate}
                className={cx("size-5 shrink-0", tone.icon)}
              />
            ) : (
              Icon && (
                <Icon
                  className={cx("size-5 shrink-0", tone.icon)}
                  aria-hidden
                />
              )
            )}
          </span>
        )}
        <div className="min-w-0 flex-1 space-y-0.5">
          {title && <p className="text-body-semibold leading-5">{title}</p>}
          {children && (
            <div
              className={cx(
                "text-body-medium [&_p]:leading-5",
                status === "info" && title && "text-ink-2",
              )}
            >
              {children}
            </div>
          )}
        </div>
      </div>
      {(actions || dismiss) && (
        <div className="-my-1 flex shrink-0 items-center gap-2 self-center">
          {actions}
          {dismiss && (
            // The one close glyph, which does not draw itself on hover.
            <CloseButton
              size="sm"
              aria-label={dismiss.label}
              title={dismiss.label}
              onClick={dismiss.run}
            />
          )}
        </div>
      )}
    </div>
  );
}
