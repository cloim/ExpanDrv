import { ChevronDown } from "lucide-react";
import type { SelectHTMLAttributes } from "react";

export type SelectControlProps = SelectHTMLAttributes<HTMLSelectElement> & {
  wrapperClassName?: string;
};

export default function SelectControl({
  wrapperClassName = "",
  className = "",
  children,
  ...props
}: SelectControlProps) {
  return (
    <span className={`select-control${wrapperClassName ? ` ${wrapperClassName}` : ""}`}>
      <select className={`select-control-input${className ? ` ${className}` : ""}`} {...props}>
        {children}
      </select>
      <ChevronDown className="select-control-chevron" size={14} strokeWidth={2} aria-hidden="true" />
    </span>
  );
}
