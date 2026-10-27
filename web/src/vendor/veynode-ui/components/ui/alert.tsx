import { cva, type VariantProps } from "class-variance-authority";
import type { ComponentProps } from "react";
import { AnimatedIcon } from "../../boardui/icons/animated-icon";
import { CircleAlert, CircleCheck, Clock, Info } from "../../icons";
import { cn } from "../../lib/utils";

/**
 * shadcn/ui Alert (see packages/ui/shadcn-sources.json), drawn in Veynode
 * Soft: a flat status card that sits in the page (it never casts a shadow;
 * a floating one adds `shadow-float` through `className`). The state is its
 * tint and ink; it draws the state's icon itself (`icon={false}` for none),
 * then `AlertTitle`, an optional `AlertDescription`, and an optional
 * `AlertAction` after them, vertically centred (a button, a close control).
 *
 *   variant   tint / ink              icon
 *   neutral   working                 info
 *   done      done                    circle-check
 *   failed    failed                  circle-alert
 *   waiting   waiting                 clock
 *   working   working                 spinner (loops)
 *
 * Announced as the old notification card was: `failed` is an `alert`, the
 * rest a `status`, always atomic. A transient message is a Toast instead.
 */
const alertVariants = cva(
  [
    "group/alert relative grid w-full items-start gap-x-2.5 overflow-hidden rounded-md p-3 text-left",
    // Columns: [icon] text [action]; each exists only when its part does.
    "grid-cols-[minmax(0,1fr)] has-[>svg]:grid-cols-[auto_minmax(0,1fr)]",
    "has-data-[slot=alert-action]:grid-cols-[minmax(0,1fr)_auto]",
    "has-[>svg]:has-data-[slot=alert-action]:grid-cols-[auto_minmax(0,1fr)_auto]",
    "*:[svg]:col-start-1 *:[svg]:row-span-2 *:[svg]:row-start-1 *:[svg]:mt-0.5 *:[svg]:size-4 *:[svg]:shrink-0",
  ],
  {
    variants: {
      variant: {
        neutral: "bg-state-working-tint text-state-working-ink",
        done: "bg-state-done-tint text-state-done-ink",
        failed: "bg-state-failed-tint text-state-failed-ink",
        waiting: "bg-state-waiting-tint text-state-waiting-ink",
        working: "bg-state-working-tint text-state-working-ink",
      },
    },
    defaultVariants: { variant: "neutral" },
  },
);

type AlertVariant = NonNullable<VariantProps<typeof alertVariants>["variant"]>;

const alertIcons = {
  neutral: Info,
  done: CircleCheck,
  failed: CircleAlert,
  waiting: Clock,
} as const;

function Alert({
  className,
  variant,
  icon,
  children,
  ...props
}: ComponentProps<"div"> &
  VariantProps<typeof alertVariants> & {
    /** `false` draws no icon; otherwise the state's own. */
    icon?: false;
  }) {
  const state: AlertVariant = variant ?? "neutral";
  const Icon = state === "working" ? undefined : alertIcons[state];
  return (
    <div
      data-slot="alert"
      data-variant={state}
      role={state === "failed" ? "alert" : "status"}
      aria-atomic="true"
      className={cn(alertVariants({ variant: state }), className)}
      {...props}
    >
      {icon === false ? null : Icon ? (
        <Icon aria-hidden />
      ) : (
        <AnimatedIcon name="loader-circle" working />
      )}
      {children}
    </div>
  );
}

function AlertTitle({ className, ...props }: ComponentProps<"p">) {
  return (
    <p
      data-slot="alert-title"
      className={cn(
        "col-start-1 row-start-1 min-w-0 text-body-medium break-words [overflow-wrap:anywhere] group-has-[>svg]/alert:col-start-2",
        className,
      )}
      {...props}
    />
  );
}

function AlertDescription({ className, ...props }: ComponentProps<"p">) {
  return (
    <p
      data-slot="alert-description"
      className={cn(
        "col-start-1 row-start-2 mt-0.5 min-w-0 text-body-2-regular text-ink-2 break-words [overflow-wrap:anywhere] group-has-[>svg]/alert:col-start-2",
        className,
      )}
      {...props}
    />
  );
}

function AlertAction({ className, ...props }: ComponentProps<"div">) {
  return (
    <div
      data-slot="alert-action"
      className={cn(
        "col-start-2 row-span-2 row-start-1 -my-1 flex shrink-0 items-center gap-1 self-center group-has-[>svg]/alert:col-start-3",
        className,
      )}
      {...props}
    />
  );
}

export { Alert, AlertAction, AlertDescription, AlertTitle, alertVariants };
export type { AlertVariant };
