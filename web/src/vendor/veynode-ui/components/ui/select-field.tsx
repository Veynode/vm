import { Combobox as ComboboxPrimitive } from "@base-ui/react/combobox";
import {
  Children,
  isValidElement,
  useRef,
  useState,
  type ComponentProps,
  type ReactElement,
  type ReactNode,
  type Ref,
} from "react";
import { AnimatedIcon } from "../../boardui/icons/animated-icon";
import { Check } from "../../icons";
import { cn } from "../../lib/utils";
import {
  usePopupContainer,
  Select,
  SelectBadge,
  SelectCheck,
  SelectContent,
  SelectGroup,
  SelectItem,
  SelectLabel,
  SelectMeta,
  SelectTrigger,
  SelectValue,
  selectItemClass,
  selectListClass,
  selectPopupClass,
  type SelectSize,
} from "./select";

export { SelectBadge };

/**
 * The Select the product uses where a list of `<option>`s is at hand: option
 * markup in, a Soft select out, so a call site reads like the form it
 * replaced. It is a thin composition of the shadcn parts in `./select`
 * (`Select`, `SelectTrigger`, `SelectValue`, `SelectContent`, `SelectItem`,
 * `SelectGroup`, `SelectLabel`); a site that needs more composes those itself.
 *
 *   <SelectField aria-label="Theme" value={theme} onValueChange={setTheme}>
 *     <option value="light">Light</option>
 *     <optgroup label="Mine"><option value="x" disabled>X</option></optgroup>
 *   </SelectField>
 *
 * - `renderOption(value, label)` draws a row (and the trigger, which shows the
 *   chosen row's content); `renderMeta(value)` adds the grey note or
 *   `SelectBadge` at a row's end (not in the trigger); `renderValue` replaces
 *   what the trigger shows.
 * - A value that matches no option shows `placeholder`. An option with the
 *   value `""` is an ordinary choice, unless it is disabled: then it reads as
 *   the placeholder (grey).
 * - `searchLabel` puts a search field over the rows (a combobox popup).
 * - `className` sizes the whole field; `triggerClassName` the button;
 *   `fieldClassName` the shell around it (not with `searchLabel`);
 *   `popoverClassName` the open panel. Other props (`data-*`, `ref`) go to the
 *   field's wrapper.
 */
export type SelectFieldProps = Omit<
  ComponentProps<"div">,
  | "children"
  | "defaultValue"
  | "onChange"
  | "placeholder"
  | "ref"
  | "id"
  | "autoFocus"
> & {
  /** `<option>`s, optionally inside `<optgroup label>`s (drawn as headed groups). */
  children: ReactNode;
  value?: string | undefined;
  defaultValue?: string | undefined;
  onValueChange?: ((value: string) => void) | undefined;
  disabled?: boolean | undefined;
  required?: boolean | undefined;
  /** Draws the edge in the failed colour. */
  invalid?: boolean | undefined;
  /** Identifies the field in a submitted form. */
  name?: string | undefined;
  /** Shown while no option is chosen. */
  placeholder?: ReactNode;
  /** `md` (default) or `sm` for compact contexts. */
  size?: SelectSize | undefined;
  triggerClassName?: string | undefined;
  /** Classes for the shell around the button (its fill, height). */
  fieldClassName?: string | undefined;
  /** Classes for the open panel (e.g. constrain its width). */
  popoverClassName?: string | undefined;
  /** When given, the panel opens with a search field of this accessible name. */
  searchLabel?: string | undefined;
  renderOption?: ((value: string, label: ReactNode) => ReactNode) | undefined;
  /** A row's trailing note in the list (not the trigger): grey text or a `SelectBadge`. */
  renderMeta?: ((value: string) => ReactNode) | undefined;
  /** What the trigger shows instead of the chosen row. */
  renderValue?: ReactNode;
  /** The trigger's id: a Field's label points at it. */
  id?: string | undefined;
  autoFocus?: boolean | undefined;
  ref?: Ref<HTMLDivElement> | undefined;
};

type OptionElement = ReactElement<{
  value?: string | number;
  children?: ReactNode;
  disabled?: boolean;
}>;
type GroupElement = ReactElement<{ label?: ReactNode; children?: ReactNode }>;

const isOption = (child: unknown): child is OptionElement =>
  isValidElement(child) && child.type === "option";
const isGroup = (child: unknown): child is GroupElement =>
  isValidElement(child) && child.type === "optgroup";

type Parsed = {
  value: string;
  label: ReactNode;
  text: string;
  disabled: boolean;
};

const textOf = (children: ReactNode) =>
  Children.toArray(children)
    .filter((child) => typeof child === "string" || typeof child === "number")
    .join("");

function parse(option: OptionElement): Parsed {
  return {
    value: String(option.props.value ?? ""),
    label: option.props.children,
    text: textOf(option.props.children),
    disabled: !!option.props.disabled,
  };
}

export function SelectField({
  children,
  value,
  defaultValue,
  onValueChange,
  disabled = false,
  required = false,
  invalid = false,
  name,
  placeholder,
  size = "md",
  className,
  triggerClassName,
  fieldClassName,
  popoverClassName,
  searchLabel,
  renderOption,
  renderMeta,
  renderValue,
  id,
  autoFocus,
  ref,
  "aria-label": ariaLabel,
  "aria-labelledby": ariaLabelledBy,
  "aria-describedby": ariaDescribedBy,
  ...rest
}: SelectFieldProps) {
  const top = Children.toArray(children);
  const options = top.flatMap((child) =>
    isOption(child)
      ? [parse(child)]
      : isGroup(child)
        ? Children.toArray(child.props.children).filter(isOption).map(parse)
        : [],
  );
  const known = (candidate: string) =>
    options.some((option) => option.value === candidate);
  const shown = (option: Parsed) =>
    renderOption ? renderOption(option.value, option.label) : option.label;
  /** A disabled `""` option is the prompt ("Choose…"), not a choice. */
  const prompt = (option: Parsed) => option.value === "" && option.disabled;
  const items = options.map((option) => ({
    value: option.value,
    label: prompt(option) ? (
      <span className="text-ink-3">{shown(option)}</span>
    ) : (
      shown(option)
    ),
  }));
  const row = (option: Parsed) => {
    const meta = renderMeta?.(option.value);
    return (
      <SelectItem
        key={option.value}
        value={option.value}
        label={option.text}
        disabled={option.disabled}
        {...(meta === undefined || meta === null || meta === false
          ? {}
          : { meta })}
      >
        {shown(option)}
      </SelectItem>
    );
  };

  const controlled = value !== undefined;
  const shownValue = controlled ? (known(value) ? value : null) : undefined;
  const initial = defaultValue ?? options[0]?.value ?? "";
  // A chosen "" that is a real option reads as a value, not a placeholder.
  const chosenEmpty = options.some(
    (option) => option.value === "" && !option.disabled,
  );

  const common = {
    disabled,
    required,
    ...(name === undefined ? {} : { name }),
    size,
  };
  const change = (next: string | null) => onValueChange?.(next ?? "");
  const triggerProps = {
    ...(id === undefined ? {} : { id }),
    ...(autoFocus ? { autoFocus } : {}),
    ...(ariaLabel === undefined ? {} : { "aria-label": ariaLabel }),
    ...(ariaLabelledBy === undefined
      ? {}
      : { "aria-labelledby": ariaLabelledBy }),
    ...(ariaDescribedBy === undefined
      ? {}
      : { "aria-describedby": ariaDescribedBy }),
    ...(invalid ? { "aria-invalid": true } : {}),
    className: cn(chosenEmpty && "data-placeholder:text-ink", triggerClassName),
  };

  return (
    <div
      ref={ref}
      {...rest}
      data-slot="select-field-root"
      className={cn("flex flex-col", className ?? "w-full")}
    >
      {searchLabel ? (
        <SearchableSelect
          options={options}
          items={items}
          shown={shown}
          value={shownValue}
          defaultValue={initial}
          onChange={change}
          common={common}
          trigger={triggerProps}
          placeholder={placeholder}
          renderValue={renderValue}
          renderMeta={renderMeta}
          searchLabel={searchLabel}
          popoverClassName={popoverClassName}
        />
      ) : (
        <Select
          items={items}
          {...common}
          {...(shownValue === undefined
            ? { defaultValue: initial }
            : { value: shownValue })}
          onValueChange={change}
        >
          <SelectTrigger {...triggerProps} fieldClassName={fieldClassName}>
            <SelectValue
              {...(placeholder === undefined ? {} : { placeholder })}
            >
              {renderValue}
            </SelectValue>
          </SelectTrigger>
          <SelectContent
            className={popoverClassName}
            {...(ariaLabel === undefined ? {} : { "aria-label": ariaLabel })}
            {...(ariaLabelledBy === undefined
              ? {}
              : { "aria-labelledby": ariaLabelledBy })}
          >
            {top.map((child, index) =>
              isOption(child) ? (
                row(parse(child))
              ) : isGroup(child) ? (
                <SelectGroup key={`group-${index}`}>
                  <SelectLabel>{child.props.label}</SelectLabel>
                  {Children.toArray(child.props.children)
                    .filter(isOption)
                    .map((option) => row(parse(option)))}
                </SelectGroup>
              ) : null,
            )}
          </SelectContent>
        </Select>
      )}
    </div>
  );
}

type Item = { value: string; label: ReactNode };

/**
 * The same field with a search over its rows: Base UI's combobox (a select
 * cannot take typed text, its keys are type-ahead). The trigger is a button;
 * the panel opens with a search field in it and a list that filters as you
 * type.
 */
function SearchableSelect({
  options,
  items,
  shown,
  value,
  defaultValue,
  onChange,
  common,
  trigger,
  placeholder,
  renderValue,
  renderMeta,
  searchLabel,
  popoverClassName,
}: {
  options: Parsed[];
  items: Item[];
  shown: (option: Parsed) => ReactNode;
  value: string | null | undefined;
  defaultValue: string;
  onChange: (value: string | null) => void;
  common: {
    disabled: boolean;
    required: boolean;
    name?: string;
    size: SelectSize;
  };
  trigger: { id?: string; className: string } & Record<string, unknown>;
  placeholder: ReactNode;
  renderValue: ReactNode;
  renderMeta: ((value: string) => ReactNode) | undefined;
  searchLabel: string;
  popoverClassName: string | undefined;
}) {
  const { size } = common;
  const anchor = useRef<HTMLElement | null>(null);
  const [open, setOpen] = useState(false);
  const byValue = new Map(options.map((option) => [option.value, option]));
  const label = (candidate: string) =>
    byValue.get(candidate)?.text ?? candidate;
  const labelNode = new Map(items.map((item) => [item.value, item.label]));
  const container = usePopupContainer(anchor);
  const { className: triggerClass, ...triggerRest } = trigger;
  return (
    <ComboboxPrimitive.Root
      items={options.map((option) => option.value)}
      itemToStringLabel={label}
      modal={false}
      onOpenChange={setOpen}
      disabled={common.disabled}
      required={common.required}
      {...(common.name === undefined ? {} : { name: common.name })}
      {...(value === undefined ? { defaultValue } : { value })}
      onValueChange={(next) => onChange(next)}
    >
      <div
        data-slot="select-field"
        data-size={size}
        className={cn(
          "field-shell group/select relative flex w-full min-w-0 items-center",
          "has-[[data-popup-open]]:bg-surface! has-[[data-popup-open]]:shadow-field-ring",
          size === "sm" ? "h-7 rounded-sm" : "h-9 rounded-md",
        )}
      >
        <ComboboxPrimitive.Trigger
          ref={(node) => {
            anchor.current = node;
          }}
          data-slot="select-trigger"
          data-select-trigger=""
          data-size={size}
          {...triggerRest}
          className={cn(
            "flex size-full min-w-0 cursor-pointer items-center justify-between bg-transparent text-left text-ink",
            "outline-none select-none disabled:cursor-not-allowed disabled:text-ink-3 data-placeholder:text-ink-3",
            size === "sm"
              ? "gap-1.5 pl-2.5 pr-2 text-body-2-regular"
              : "gap-2 pl-3 pr-2.5 text-body-regular",
            triggerClass,
          )}
        >
          <span
            data-slot="select-value"
            className={cn(
              "flex min-w-0 flex-1 items-center gap-2 truncate text-left",
              size === "sm" && "gap-1.5",
            )}
          >
            {renderValue ?? (
              <ComboboxPrimitive.Value
                {...(placeholder === undefined ? {} : { placeholder })}
              >
                {(selected: string | null) =>
                  selected === null || selected === undefined
                    ? placeholder
                    : (labelNode.get(selected) ?? selected)
                }
              </ComboboxPrimitive.Value>
            )}
          </span>
          <span className="flex shrink-0 text-ink-2">
            <AnimatedIcon
              name="chevron-down"
              on={open}
              className="size-4 shrink-0"
            />
          </span>
        </ComboboxPrimitive.Trigger>
      </div>
      <ComboboxPrimitive.Portal container={container}>
        <ComboboxPrimitive.Positioner
          side="bottom"
          align="start"
          sideOffset={4}
          collisionPadding={16}
          positionMethod="fixed"
          className="isolate z-menu"
        >
          <ComboboxPrimitive.Popup
            data-slot="select-content"
            className={cn(selectPopupClass, popoverClassName)}
          >
            <div className="field-shell mx-0.5 mb-1.5 mt-0.5 flex h-7 shrink-0 items-center rounded-sm px-2">
              <ComboboxPrimitive.Input
                aria-label={searchLabel}
                placeholder={searchLabel}
                autoComplete="off"
                className="m-0 min-w-0 flex-1 border-0 bg-transparent p-0 font-sans text-body-2-regular text-ink outline-none placeholder:text-ink-3"
              />
            </div>
            <ComboboxPrimitive.List
              className={selectListClass}
              {...(trigger["aria-label"] === undefined
                ? {}
                : { "aria-label": String(trigger["aria-label"]) })}
              {...(trigger["aria-labelledby"] === undefined
                ? {}
                : { "aria-labelledby": String(trigger["aria-labelledby"]) })}
            >
              {(candidate: string) => {
                const option = byValue.get(candidate);
                if (!option) return null;
                const meta = renderMeta?.(candidate);
                return (
                  <ComboboxPrimitive.Item
                    key={candidate}
                    value={candidate}
                    disabled={option.disabled}
                    data-slot="select-item"
                    className={selectItemClass(size)}
                  >
                    <span className="flex min-w-0 flex-1 items-center gap-3 truncate">
                      {shown(option)}
                    </span>
                    <SelectMeta>{meta}</SelectMeta>
                    <SelectCheck>
                      <ComboboxPrimitive.ItemIndicator>
                        <Check aria-hidden className="size-4 text-ink" />
                      </ComboboxPrimitive.ItemIndicator>
                    </SelectCheck>
                  </ComboboxPrimitive.Item>
                );
              }}
            </ComboboxPrimitive.List>
          </ComboboxPrimitive.Popup>
        </ComboboxPrimitive.Positioner>
      </ComboboxPrimitive.Portal>
    </ComboboxPrimitive.Root>
  );
}
