import { useRef } from "react";

/**
 * Whether an icon changed since the control mounted, and a key that is new
 * for each change: `<Icon key={swap} className={swap && "icon-swap-in"} />`
 * pops the new icon in (ds 「复制、保存成功」: Copy → Check and back, 160ms on
 * the pop curve, the check drawing itself on). The first icon never
 * animates, so history, remounts and reloads stay still.
 */
export function useIconSwap(icon: unknown): string | undefined {
  const last = useRef(icon);
  const changes = useRef(0);
  if (last.current !== icon) {
    last.current = icon;
    changes.current += 1;
  }
  return changes.current ? `swap-${changes.current}` : undefined;
}
