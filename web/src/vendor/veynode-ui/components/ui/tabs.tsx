import { Tabs as TabsPrimitive } from "@base-ui/react/tabs";
import {
  createContext,
  useContext,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import type { Ref } from "react";
import { cn } from "../../lib/utils";

/**
 * shadcn/ui Tabs (Base UI flavour, see packages/ui/shadcn-sources.json),
 * drawn in Veynode Soft. Two looks of one list, chosen with `variant`:
 *
 *   line  (default) underline tabs on a 1px `line` baseline. The chosen tab
 *         is `ink` and the rest `ink-2`, semibold; a 3px `action` bar slides
 *         under it (in `vertical` orientation: down the left edge, the list
 *         then borders on the left). Tabs sit 4px apart, 40px tall.
 *   pill  Veynode Soft's segmented control, the same as the sidebar's places
 *         (`SidebarTabs`): a `surface-2` track with a 3px inset, and a
 *         `surface` thumb, neither with a hairline, that slides to the chosen
 *         option (200ms, the standard curve). Hover changes only the tab's
 *         colour; the choice is the thumb moving. Tabs
 *         are 30px to see and 44px to touch (the list is 44px tall around
 *         them). The list is never wider than its container: where the tabs
 *         do not fit (three words on a phone) it scrolls sideways, the chosen
 *         tab is brought into view and no tab is cut off. The thumb moves
 *         only when the choice changes: placing it on first show, on resize
 *         or after a hidden panel reopens is immediate. `size="sm"` makes the
 *         tabs 22px (a 28px track) in caption-sized text, for a choice at a
 *         settings row's end.
 *
 * Arrow keys move between tabs and choose at once (`activateOnFocus`); Home
 * and End jump. A switcher without panels needs no `TabsContent`:
 *
 *   <Tabs value={view} onValueChange={setView} className="contents">
 *     <TabsList variant="pill" aria-label="Views">
 *       <TabsTrigger value="installed">Installed</TabsTrigger>
 *       <TabsTrigger value="discover">Discover</TabsTrigger>
 *     </TabsList>
 *   </Tabs>
 *
 * `className="contents"` on the root keeps it out of the surrounding layout
 * (the list then takes its place directly); the pill list's own `className`
 * goes on its scrolling frame.
 */

type TabsListVariant = "line" | "pill";

type TabsListSize = "md" | "sm";

const VariantContext = createContext<TabsListVariant>("line");
const SizeContext = createContext<TabsListSize>("md");

type WithClass<P> = Omit<P, "className"> & { className?: string | undefined };

function Tabs({
  className,
  orientation = "horizontal",
  ...props
}: WithClass<TabsPrimitive.Root.Props>) {
  return (
    <TabsPrimitive.Root
      data-slot="tabs"
      orientation={orientation}
      className={cn(
        "flex gap-4 data-[orientation=horizontal]:flex-col data-[orientation=vertical]:flex-row",
        className,
      )}
      {...props}
    />
  );
}

type TabsListProps = WithClass<TabsPrimitive.List.Props> & {
  variant?: TabsListVariant;
  /** The pill's size; the line tabs have one. */
  size?: TabsListSize;
  ref?: Ref<HTMLDivElement>;
};

function setRef<T>(ref: Ref<T> | undefined, node: T | null) {
  if (typeof ref === "function") ref(node);
  else if (ref) (ref as { current: T | null }).current = node;
}

function TabsList({ variant = "line", size = "md", ...props }: TabsListProps) {
  return (
    <VariantContext.Provider value={variant}>
      <SizeContext.Provider value={size}>
        {variant === "pill" ? <PillList {...props} /> : <LineList {...props} />}
      </SizeContext.Provider>
    </VariantContext.Provider>
  );
}

function LineList({
  className,
  children,
  ref,
  activateOnFocus = true,
  ...props
}: Omit<TabsListProps, "variant" | "size">) {
  return (
    <TabsPrimitive.List
      ref={ref}
      data-slot="tabs-list"
      data-variant="line"
      activateOnFocus={activateOnFocus}
      className={cn(
        "relative flex w-full items-center gap-1 border-b border-line",
        "data-[orientation=vertical]:flex-col data-[orientation=vertical]:items-stretch data-[orientation=vertical]:border-b-0 data-[orientation=vertical]:border-l",
        className,
      )}
      {...props}
    >
      {children}
      <TabsPrimitive.Indicator
        data-slot="tabs-indicator"
        className={cn(
          "pointer-events-none absolute bg-action transition-[translate,width,height] duration-200 ease-standard motion-reduce:transition-none",
          "data-[orientation=horizontal]:bottom-[-1px] data-[orientation=horizontal]:left-0 data-[orientation=horizontal]:h-[3px] data-[orientation=horizontal]:w-(--active-tab-width) data-[orientation=horizontal]:translate-x-(--active-tab-left) data-[orientation=horizontal]:rounded-t-full",
          "data-[orientation=vertical]:top-0 data-[orientation=vertical]:left-[-1px] data-[orientation=vertical]:h-(--active-tab-height) data-[orientation=vertical]:w-[3px] data-[orientation=vertical]:translate-y-(--active-tab-top) data-[orientation=vertical]:rounded-r-full",
        )}
      />
    </TabsPrimitive.List>
  );
}

const useIsomorphicLayoutEffect =
  typeof window !== "undefined" ? useLayoutEffect : useEffect;

type Thumb = {
  left: number;
  top: number;
  width: number;
  height: number;
  animate: boolean;
};

/**
 * Where the pill thumb sits: on the chosen tab, measured in the list's own
 * coordinates. It animates only when the chosen tab changes. A hidden list
 * has no geometry, so reopening it is a first placement rather than a move
 * from a zero-sized thumb. Where the tabs scroll, the chosen one is brought
 * into view.
 */
function usePillThumb(list: { current: HTMLDivElement | null }) {
  const [thumb, setThumb] = useState<Thumb | null>(null);

  useIsomorphicLayoutEffect(() => {
    const el = list.current;
    if (!el) return;
    let previousSelected: HTMLElement | null = null;
    const measure = () => {
      const selected = el.querySelector<HTMLElement>(
        "[role='tab'][aria-selected='true']",
      );
      if (!selected || !selected.offsetWidth || !selected.offsetHeight) {
        previousSelected = null;
        setThumb(null);
        return;
      }
      const next = {
        left: selected.offsetLeft,
        top: selected.offsetTop,
        width: selected.offsetWidth,
        height: selected.offsetHeight,
        animate: previousSelected !== null && previousSelected !== selected,
      };
      const scroller = el.parentElement;
      if (scroller && scroller.scrollWidth > scroller.clientWidth) {
        const start = selected.offsetLeft - 3;
        const end = selected.offsetLeft + selected.offsetWidth + 3;
        if (start < scroller.scrollLeft) scroller.scrollLeft = start;
        else if (end > scroller.scrollLeft + scroller.clientWidth)
          scroller.scrollLeft = end - scroller.clientWidth;
      }
      previousSelected = selected;
      setThumb((current) => {
        // ResizeObserver may follow a selection mutation in the same frame.
        // Keep that transition when the geometry has not changed again.
        if (
          current &&
          current.left === next.left &&
          current.top === next.top &&
          current.width === next.width &&
          current.height === next.height
        )
          return current;
        return next;
      });
    };
    measure();
    const ro = new ResizeObserver(measure);
    const observeTabs = () => {
      ro.disconnect();
      ro.observe(el);
      if (el.parentElement) ro.observe(el.parentElement);
      for (const tab of el.querySelectorAll("[role='tab']")) ro.observe(tab);
    };
    observeTabs();
    const mo = new MutationObserver((records) => {
      if (records.some((record) => record.type === "childList")) observeTabs();
      measure();
    });
    mo.observe(el, {
      attributes: true,
      childList: true,
      subtree: true,
      attributeFilter: ["aria-selected"],
    });
    return () => {
      mo.disconnect();
      ro.disconnect();
    };
  }, [list]);

  return thumb;
}

function PillList({
  className,
  children,
  ref,
  activateOnFocus = true,
  ...props
}: Omit<TabsListProps, "variant" | "size">) {
  const listRef = useRef<HTMLDivElement | null>(null);
  const thumb = usePillThumb(listRef);
  const setRefs = (node: HTMLDivElement | null) => {
    listRef.current = node;
    setRef(ref, node);
  };
  // The frame is 44px tall (the pill's 36 and 4px above and below, taken
  // back by the negative margin), so a tab's touch area fits inside it.
  return (
    <div
      data-slot="tabs-scroller"
      className={cn(
        "-my-1 inline-flex max-w-full overflow-x-auto overflow-y-hidden overscroll-x-contain py-1 [scrollbar-width:none] [&::-webkit-scrollbar]:hidden",
        className,
      )}
    >
      <TabsPrimitive.List
        ref={setRefs}
        data-slot="tabs-list"
        data-variant="pill"
        activateOnFocus={activateOnFocus}
        // In a settings group (itself surface-2) the track takes the canvas, so it still reads.
        className="relative inline-flex shrink-0 items-center gap-0.5 rounded-full bg-surface-2 p-[3px] in-data-settings-group:bg-canvas"
        {...props}
      >
        {thumb && (
          <span
            aria-hidden
            data-slot="tabs-thumb"
            className={cn(
              "pointer-events-none absolute top-0 left-0 rounded-full bg-surface",
              thumb.animate &&
                "transition-[transform,width,height] duration-[var(--motion-slow)] ease-standard motion-reduce:transition-none",
            )}
            style={{
              transform: `translate(${thumb.left}px, ${thumb.top}px)`,
              width: thumb.width,
              height: thumb.height,
            }}
          />
        )}
        {children}
      </TabsPrimitive.List>
    </div>
  );
}

function TabsTrigger({
  className,
  ...props
}: WithClass<TabsPrimitive.Tab.Props>) {
  const variant = useContext(VariantContext);
  const small = useContext(SizeContext) === "sm";
  return (
    <TabsPrimitive.Tab
      data-slot="tabs-trigger"
      className={cn(
        "relative inline-flex cursor-pointer items-center whitespace-nowrap text-ink-2 select-none",
        variant === "pill" && small
          ? "text-body-2-semibold"
          : "text-body-semibold",
        "outline-none transition-colors duration-[var(--button-transition-ms)] ease-standard data-active:text-ink",
        "focus-visible:outline-2 focus-visible:outline-ring",
        "data-disabled:cursor-not-allowed data-disabled:opacity-45",
        "[&_svg]:pointer-events-none [&_svg]:shrink-0 [&_svg:not([class*='size-'])]:size-4",
        variant === "pill"
          ? // 30px to see, 44px to touch (the list is 44px tall around it).
            cn(
              "z-lift shrink-0 gap-1.5 rounded-full before:absolute before:inset-x-0 before:content-[''] pointer-fine:before:hidden hover:text-ink focus-visible:outline-offset-2",
              small
                ? "h-[22px] px-2.5 before:-inset-y-[11px] max-narrow:px-2"
                : "h-[30px] px-4 before:-inset-y-[7px]",
            )
          : "min-h-10 gap-2 px-3 py-2 focus-visible:rounded-sm focus-visible:-outline-offset-2",
        className,
      )}
      {...props}
    />
  );
}

function TabsContent({
  className,
  ...props
}: WithClass<TabsPrimitive.Panel.Props>) {
  return (
    <TabsPrimitive.Panel
      data-slot="tabs-content"
      className={cn(
        // The panel being left is gone at once: for the frame it lingers beside
        // the one arriving (`data-starting-style`), two would stack and the
        // content below would jump.
        "[&:has(~[data-slot=tabs-content][data-starting-style])]:hidden [[data-slot=tabs-content][data-starting-style]~&]:hidden outline-none focus-visible:rounded-sm focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ring",
        className,
      )}
      {...props}
    />
  );
}

export { Tabs, TabsContent, TabsList, TabsTrigger };
