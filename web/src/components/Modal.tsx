import { useRef, type ReactNode } from "react";
import { cx } from "../util";
import { useModalLayer } from "../overlays";
import { IconX } from "./Icons";

export function Modal({
  title,
  onClose,
  children,
  small,
  wide,
}: {
  title: string;
  onClose: () => void;
  children: ReactNode;
  small?: boolean;
  wide?: boolean;
}) {
  const ref = useRef<HTMLDivElement>(null);
  useModalLayer(ref, onClose);
  return (
    <div className="modal-backdrop" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div ref={ref} tabIndex={-1} className={cx("modal", small && "small", wide && "wide")} role="dialog" aria-modal="true" aria-label={title}>
        <div className="modal-head">
          <h2>{title}</h2>
          <button className="icon-btn" onClick={onClose} aria-label="Close">
            <IconX />
          </button>
        </div>
        <div className="modal-body">{children}</div>
      </div>
    </div>
  );
}
