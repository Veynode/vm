import {
  Children,
  cloneElement,
  isValidElement,
  useId,
  type ReactNode,
  type ReactElement,
} from "react";
import { Label } from "../components/ui/label";
import { cx } from "../boardui/utils/cx";

type Associable = ReactElement<{
  id?: string;
  "aria-labelledby"?: string;
  "aria-label"?: string | undefined;
}>;

/** Give a control the shared field id and label association, including Select triggers. */
export function associateControl(child: ReactNode, id: string) {
  if (!isValidElement(child)) return child;
  return cloneElement(child as Associable, {
    id,
    "aria-labelledby": `${id}-label`,
    "aria-label": undefined,
  });
}

/**
 * Associate the visible BoardUI label with the first field, including Select
 * triggers. `trailingAction` sits right after the label (an ⓘ, "Optional");
 * `end` at the far end of the label row, for a quiet link such as
 * "Create a key". Neither changes the row's height.
 */
export function Field({
  label,
  children,
  className,
  trailingAction,
  end,
}: {
  label: ReactNode;
  children: ReactNode;
  className?: string;
  trailingAction?: ReactNode;
  end?: ReactNode;
}) {
  const id = useId();
  let associated = false;
  return (
    <div className={cx("flex flex-col gap-1.5 text-body-regular", className)}>
      {trailingAction || end ? <div className="flex items-center gap-1">
        <Label id={`${id}-label`} htmlFor={id}>{label}</Label>
        {trailingAction ? <span className="inline-flex h-0 items-center">{trailingAction}</span> : null}
        {end ? <span className="ml-auto inline-flex h-0 min-w-0 items-center">{end}</span> : null}
      </div> : <Label id={`${id}-label`} htmlFor={id}>{label}</Label>}
      {Children.map(children, (child) => {
        if (associated || !isValidElement(child)) return child;
        associated = true;
        return associateControl(child, id);
      })}
    </div>
  );
}
