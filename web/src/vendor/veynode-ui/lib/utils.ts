import { clsx, type ClassValue } from "clsx";
import { cx } from "../boardui/utils/cx";

/**
 * shadcn/ui's `cn`: clsx, then tailwind-merge. The merge is the extended one
 * from `cx`, which knows Veynode's text recipes, shadows and radii.
 */
export function cn(...inputs: ClassValue[]) {
  return cx(clsx(inputs));
}
