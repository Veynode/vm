import { Button as ButtonPrimitive } from "@base-ui/react/button";
import { cva, type VariantProps } from "class-variance-authority";
import type {
  AnchorHTMLAttributes,
  ComponentType,
  ReactNode,
  Ref,
} from "react";
import {
  AnimatedIcon,
  type AnimatedIconProps,
} from "../../boardui/icons/animated-icon";
import { useIconSwap } from "../../boardui/utils/use-icon-swap";
import { cn } from "../../lib/utils";

/**
 * shadcn/ui Button (Base UI flavour, see packages/ui/shadcn-sources.json),
 * drawn in Veynode Soft: every button is a pill; one `default` (ink) per
 * screen, `outline` a surface with a 1px hairline, `ghost` clear until
 * hovered, `secondary` a quiet grey fill, `destructive` the failed colour.
 * Nothing casts a shadow. Pressing shrinks it to .97; disabled is the same
 * button at 45%.
 *
 *            height   padding   icon   text
 *   lg       44       20        20     15 semibold (chat)
 *   default  36       16        16     14 semibold (body)
 *   sm       28       12        16     13 semibold (small)
 *   xs       24       8         14     12 semibold (caption)
 *
 * The `icon*` sizes are the round squares of the same heights, for a button
 * with no label: `<Button size="icon" leadingIcon={Plus} aria-label="Add" />`.
 *
 * Icons are drawn by the component itself through `leadingIcon` /
 * `trailingIcon`, so the consumer cannot pass the wrong size. Pass a
 * component reference (`Plus`, not `<Plus />`). `leadingAnimate` draws an
 * AnimatedIcon in the leading place instead, for an icon that moves with the
 * button's action:
 *   <Button leadingAnimate={{ name: "refresh-cw", play: retries }}>Retry</Button>
 * (`trailingAnimate` likewise for the trailing icon.)
 *
 * On a touch screen every size has 44px to press: an invisible hit area
 * (`before:`) around the drawn button, off for a mouse (`pointer-fine`). The
 * drawn size and the row's height do not change.
 */
const buttonVariants = cva(
  [
    "group/button relative inline-flex min-w-0 shrink-0 items-center justify-center gap-1.5 whitespace-nowrap rounded-full",
    "before:absolute before:content-[''] pointer-fine:before:hidden",
    "font-sans select-none cursor-pointer button-press-motion",
    "outline-none focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring",
    "disabled:cursor-not-allowed disabled:opacity-45 aria-disabled:cursor-not-allowed aria-disabled:opacity-45",
  ],
  {
    variants: {
      variant: {
        default: "bg-button-primary text-primary-foreground",
        destructive: "bg-button-danger text-destructive-foreground",
        outline:
          "bg-background text-foreground inset-ring inset-ring-border hover:bg-muted active:bg-muted",
        secondary:
          "bg-secondary text-secondary-foreground hover:bg-field active:bg-field",
        ghost:
          "bg-transparent text-foreground hover:bg-muted active:bg-muted aria-expanded:bg-muted disabled:bg-transparent aria-disabled:bg-transparent",
        link: "bg-transparent text-foreground underline-offset-4 hover:underline",
      },
      size: {
        default: "h-9 px-4 text-body-semibold before:-inset-1",
        xs: "h-6 px-2 text-caption-1-semibold before:-inset-2.5",
        sm: "h-7 px-3 text-body-2-semibold before:-inset-2",
        lg: "h-11 px-5 text-chat-semibold",
        icon: "size-9 p-0 before:-inset-1",
        "icon-xs": "size-6 p-0 before:-inset-2.5",
        "icon-sm": "size-7 p-0 before:-inset-2",
        "icon-lg": "size-11 p-0",
      },
    },
    defaultVariants: { variant: "default", size: "default" },
  },
);

type ButtonSize = NonNullable<VariantProps<typeof buttonVariants>["size"]>;

/** The glyph's size: a labelled button's, or the larger one an icon-only button carries. */
const iconSizes: Record<ButtonSize, string> = {
  lg: "size-5 shrink-0",
  default: "size-4 shrink-0",
  sm: "size-4 shrink-0",
  xs: "size-3.5 shrink-0",
  icon: "size-5 shrink-0",
  "icon-lg": "size-5 shrink-0",
  "icon-sm": "size-4 shrink-0",
  "icon-xs": "size-3.5 shrink-0",
};

const labelClass = "inline-flex min-w-0 items-center overflow-hidden";

type IconComponent = ComponentType<{
  className?: string | undefined;
  "aria-hidden"?: boolean | "true" | "false" | undefined;
}>;

type IconAnimate = Omit<AnimatedIconProps, "className" | "size"> | undefined;

interface ButtonOwnProps extends VariantProps<typeof buttonVariants> {
  leadingIcon?: IconComponent | undefined;
  leadingAnimate?: IconAnimate;
  trailingIcon?: IconComponent | undefined;
  /** An AnimatedIcon in the trailing place, e.g. a chevron that turns as its disclosure opens. */
  trailingAnimate?: IconAnimate;
  children?: ReactNode;
}

export type ButtonProps = Omit<
  ButtonPrimitive.Props,
  "children" | "className"
> & {
  className?: string | undefined;
} & ButtonOwnProps & {
    /**
     * The action it started is under way: a spinner takes the label's place
     * (the label stays, unseen, so the button keeps its size and its name)
     * and the button is disabled.
     */
    pending?: boolean;
  };

export type ButtonLinkProps = Omit<
  AnchorHTMLAttributes<HTMLAnchorElement>,
  "children"
> &
  ButtonOwnProps & { ref?: Ref<HTMLAnchorElement> };

function isIconOnly(size: ButtonSize) {
  return size.startsWith("icon");
}

/** The leading icon, the label and the trailing icon: what both Button and ButtonLink hold. */
function ButtonContent({
  size,
  leadingIcon: Leading,
  leadingAnimate,
  trailingIcon: Trailing,
  trailingAnimate,
  children,
}: ButtonOwnProps & { size: ButtonSize }) {
  // A leading icon that changes (Copy → Check) pops in; the first never does.
  const swap = useIconSwap(Leading);
  const iconOnly = isIconOnly(size);
  const icon = iconSizes[size];
  return (
    <>
      {leadingAnimate ? (
        <AnimatedIcon {...leadingAnimate} className={icon} />
      ) : Leading ? (
        <Leading
          key={swap}
          className={cn(icon, swap && "icon-swap-in")}
          aria-hidden
        />
      ) : null}
      {!iconOnly && children !== undefined && children !== null && (
        <span className={labelClass}>{children}</span>
      )}
      {!iconOnly && trailingAnimate ? (
        <AnimatedIcon {...trailingAnimate} className={icon} />
      ) : !iconOnly && Trailing ? (
        <Trailing className={icon} aria-hidden />
      ) : null}
    </>
  );
}

function Button({
  variant,
  size = "default",
  leadingIcon,
  leadingAnimate,
  trailingIcon,
  trailingAnimate,
  pending = false,
  children,
  className,
  type = "button",
  disabled,
  ...props
}: ButtonProps) {
  const resolved = size ?? "default";
  return (
    <ButtonPrimitive
      data-slot="button"
      type={type}
      className={cn(
        buttonVariants({ variant, size }),
        // The label gives way to a spinner in place: the content stays laid
        // out (unseen, still the button's name) so the button never resizes.
        pending && "relative [&>*:not([data-button-spinner])]:opacity-0",
        className,
      )}
      {...props}
      disabled={disabled || pending}
      {...(pending ? { "aria-busy": true } : {})}
    >
      {pending && (
        <AnimatedIcon
          name="loader-circle"
          working
          data-button-spinner=""
          className={cn("absolute inset-0 m-auto", iconSizes[resolved])}
        />
      )}
      <ButtonContent
        size={resolved}
        leadingIcon={leadingIcon}
        leadingAnimate={leadingAnimate}
        trailingIcon={trailingIcon}
        trailingAnimate={trailingAnimate}
      >
        {children}
      </ButtonContent>
    </ButtonPrimitive>
  );
}

/** Anchor counterpart to Button for navigational actions. */
function ButtonLink({
  variant,
  size = "default",
  leadingIcon,
  leadingAnimate,
  trailingIcon,
  trailingAnimate,
  children,
  className,
  ref,
  ...props
}: ButtonLinkProps) {
  const resolved = size ?? "default";
  return (
    <a
      ref={ref}
      data-slot="button"
      className={cn(buttonVariants({ variant, size }), className)}
      {...props}
    >
      <ButtonContent
        size={resolved}
        leadingIcon={leadingIcon}
        leadingAnimate={leadingAnimate}
        trailingIcon={trailingIcon}
        trailingAnimate={trailingAnimate}
      >
        {children}
      </ButtonContent>
    </a>
  );
}

export {
  Button,
  ButtonLink,
  buttonVariants,
  iconSizes as buttonIconSizes,
  labelClass as buttonLabelClass,
};
