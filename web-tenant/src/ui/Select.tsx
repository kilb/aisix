import { Children, Fragment, isValidElement, useId, useState, type ComponentProps, type ReactNode } from "react";
import { Select as SelectPrimitive, DropdownMenu } from "radix-ui";
import { Check, ChevronDown, ChevronUp } from "lucide-react";
import { cn } from "../lib/cn";
import "./dropdown.css";

export interface SelectOption {
  value: string;
  label: string;
  disabled?: boolean;
  group?: string;
}

type TriggerProps = Omit<ComponentProps<"button">, "value" | "defaultValue" | "onChange" | "children" | "size">;

export interface SelectProps extends TriggerProps {
  value: string | number;
  onValueChange: (value: string) => void;
  options?: readonly SelectOption[];
  /** 迁移已有动态选项时保留 option 描述符，弹层由 Radix 渲染。 */
  children?: ReactNode;
  size?: "compact";
  placeholder?: string;
  name?: string;
  required?: boolean;
  form?: string;
}

function optionText(children: ReactNode): string {
  return Children.toArray(children).map(child => isValidElement<{ children?: ReactNode }>(child) ? optionText(child.props.children) : String(child)).join("");
}

function optionsFromChildren(children: ReactNode, group?: string): SelectOption[] {
  return Children.toArray(children).flatMap(child => {
    if (!isValidElement<ComponentProps<"option"> & { children?: ReactNode }>(child)) return [];
    if (child.type === Fragment) return optionsFromChildren(child.props.children, group);
    if (child.type === "optgroup") return optionsFromChildren(child.props.children, child.props.label);
    if (child.type !== "option") return [];
    const label = child.props.label ?? optionText(child.props.children);
    return [{ value: String(child.props.value ?? label), label, disabled: child.props.disabled, group }];
  });
}

/** 单选沿用原值与表单语义，弹层独立于表格滚动容器。 */
export function Select({ value, onValueChange, options, children, size, placeholder = "请选择", name, required, form, disabled, className, ...triggerProps }: SelectProps) {
  const choices = options ?? optionsFromChildren(children);
  const selected = choices.find(option => option.value === String(value));
  const validationId = useId();
  const [validationMessage, setValidationMessage] = useState("");
  const nativeInvalid = required && String(value) === "" && validationMessage !== "";
  return <span className={cn("ui-select-field", size === "compact" && "ui-select-field-compact")} onInvalid={event => {
    event.preventDefault();
    setValidationMessage((event.target as HTMLSelectElement).validationMessage);
    event.currentTarget.querySelector<HTMLButtonElement>('[role="combobox"]')?.focus();
  }}><SelectPrimitive.Root value={String(value)} onValueChange={onValueChange} name={name} required={required} form={form} disabled={disabled}>
    <SelectPrimitive.Trigger {...triggerProps} aria-invalid={triggerProps["aria-invalid"] || nativeInvalid || undefined} aria-describedby={[triggerProps["aria-describedby"], nativeInvalid ? validationId : undefined].filter(Boolean).join(" ") || undefined} className={cn("ui-select-trigger", size === "compact" && "ui-select-compact", className)} data-value={String(value)}>
      <span className="ui-select-value"><SelectPrimitive.Value placeholder={selected?.label ?? placeholder}>{selected?.label ?? String(value)}</SelectPrimitive.Value></span>
      <SelectPrimitive.Icon className="ui-select-chevron"><ChevronDown aria-hidden="true" /></SelectPrimitive.Icon>
    </SelectPrimitive.Trigger>
    <SelectPrimitive.Portal>
      <SelectPrimitive.Content className="ui-dropdown ui-select-content" position="popper" sideOffset={4} align="start" collisionPadding={12}>
        <SelectPrimitive.ScrollUpButton className="ui-select-scroll"><ChevronUp aria-hidden="true" /></SelectPrimitive.ScrollUpButton>
        <SelectPrimitive.Viewport className="ui-dropdown-viewport">
          {choices.map((option, index) => <Fragment key={option.value}>
            {option.group && option.group !== choices[index - 1]?.group && <div className="ui-dropdown-label">{option.group}</div>}
            <SelectPrimitive.Item className="ui-dropdown-item ui-select-option" value={option.value} data-value={option.value} disabled={option.disabled} textValue={option.label}>
              <SelectPrimitive.ItemText>{option.label}</SelectPrimitive.ItemText>
              <SelectPrimitive.ItemIndicator className="ui-dropdown-check"><Check aria-hidden="true" /></SelectPrimitive.ItemIndicator>
            </SelectPrimitive.Item>
          </Fragment>)}
        </SelectPrimitive.Viewport>
        <SelectPrimitive.ScrollDownButton className="ui-select-scroll"><ChevronDown aria-hidden="true" /></SelectPrimitive.ScrollDownButton>
      </SelectPrimitive.Content>
    </SelectPrimitive.Portal>
  </SelectPrimitive.Root>{nativeInvalid && <span className="ui-select-validation" id={validationId} aria-hidden="true">{validationMessage}</span>}</span>;
}

export interface MultiSelectProps extends TriggerProps {
  value: readonly string[];
  onValueChange: (value: string[]) => void;
  options: readonly SelectOption[];
  placeholder?: string;
  size?: "compact";
}

/** 多选保留已选项并保持菜单展开，Escape 和外点统一由 Radix 管理。 */
export function MultiSelect({ value, onValueChange, options, placeholder = "请选择", size, disabled, className, ...triggerProps }: MultiSelectProps) {
  const labels = value.map(item => options.find(option => option.value === item)?.label ?? item);
  return <DropdownMenu.Root modal={false}>
    <DropdownMenu.Trigger {...triggerProps} disabled={disabled} className={cn("ui-select-trigger", size === "compact" && "ui-select-compact", className)}>
      <span className="ui-select-value" title={labels.join("、")}>{labels.length ? labels.join("、") : placeholder}</span>
      <ChevronDown className="ui-select-chevron" aria-hidden="true" />
    </DropdownMenu.Trigger>
    <DropdownMenu.Portal>
      <DropdownMenu.Content className="ui-dropdown ui-multiselect-content" sideOffset={4} align="start" collisionPadding={12} loop>
        {options.map(option => <DropdownMenu.CheckboxItem key={option.value} className="ui-dropdown-item ui-select-option" checked={value.includes(option.value)} disabled={option.disabled} onSelect={event => event.preventDefault()} onCheckedChange={checked => onValueChange(checked ? [...value, option.value] : value.filter(item => item !== option.value))} textValue={option.label}>
          <span>{option.label}</span><DropdownMenu.ItemIndicator className="ui-dropdown-check"><Check aria-hidden="true" /></DropdownMenu.ItemIndicator>
        </DropdownMenu.CheckboxItem>)}
      </DropdownMenu.Content>
    </DropdownMenu.Portal>
  </DropdownMenu.Root>;
}
