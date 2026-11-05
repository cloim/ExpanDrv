import React, { useCallback, useEffect, useId, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { X } from 'lucide-react';
import { isTopModal, popModal, pushModal } from './modalStack.js';
import { RESIZE_HANDLES, useModalGeometry } from './useModalGeometry.js';

const FOCUSABLE_SELECTOR =
  'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

export function Modal({
  isOpen,
  onClose,
  title,
  children,
  footer,
  size = 'md',
  draggable = true,
  resizable = false,
  storageKey,
  closeOnOverlayClick = true,
  closeOnEscape = true,
  initialFocusRef,
  minWidth = 320,
  minHeight = 200,
  className = '',
}) {
  const panelRef = useRef(null);
  const previouslyFocusedRef = useRef(null);
  const overlayPointerDownRef = useRef(false);
  const modalId = useId();
  const [zIndex, setZIndex] = useState(null);

  const { panelStyle, headerProps, getResizeHandleProps, canDrag, canResize } = useModalGeometry({
    panelRef,
    isOpen,
    draggable,
    resizable,
    storageKey,
    minWidth,
    minHeight,
  });

  // 스택 등록과 해제. 등록 시점에 받은 z-index로 중첩 순서를 유지한다.
  useEffect(() => {
    if (!isOpen) {
      return;
    }
    setZIndex(pushModal(modalId));
    return () => popModal(modalId);
  }, [isOpen, modalId]);

  // 열릴 때 첫 포커스를 옮기고, 닫힐 때 열기 직전 요소로 되돌린다. 되돌리지 않으면 모달을
  // 닫은 뒤 포커스가 body로 떨어져 키보드 사용자가 맥락을 잃는다.
  useEffect(() => {
    if (!isOpen) {
      return;
    }
    previouslyFocusedRef.current = document.activeElement;
    const timer = window.setTimeout(() => {
      const target =
        initialFocusRef?.current ?? panelRef.current?.querySelector(FOCUSABLE_SELECTOR) ?? panelRef.current;
      target?.focus();
    }, 0);
    return () => {
      window.clearTimeout(timer);
      const previous = previouslyFocusedRef.current;
      if (previous instanceof HTMLElement && document.contains(previous)) {
        previous.focus();
      }
    };
  }, [isOpen, initialFocusRef]);

  // ESC와 Tab. ESC는 맨 위 모달만 반응해, 중첩 상태에서 한 번에 하나씩 닫히게 한다.
  useEffect(() => {
    if (!isOpen) {
      return;
    }
    const onKeyDown = (event) => {
      if (event.key === 'Escape' && closeOnEscape && isTopModal(modalId)) {
        event.stopPropagation();
        onClose();
        return;
      }
      if (event.key === 'Tab' && isTopModal(modalId)) {
        trapFocus(event, panelRef.current);
      }
    };
    document.addEventListener('keydown', onKeyDown, true);
    return () => document.removeEventListener('keydown', onKeyDown, true);
  }, [isOpen, closeOnEscape, onClose, modalId]);

  // 오버레이 클릭 닫기. pointerdown과 click이 모두 오버레이에서 일어났을 때만 닫는다.
  // 본문에서 드래그를 시작해 오버레이에서 손을 뗀 경우(텍스트 선택, 헤더 드래그)에 모달이
  // 닫혀버리는 것을 막기 위한 조건이다.
  const onOverlayPointerDown = useCallback((event) => {
    overlayPointerDownRef.current = event.target === event.currentTarget;
  }, []);

  const onOverlayClick = useCallback(
    (event) => {
      const startedOnOverlay = overlayPointerDownRef.current;
      overlayPointerDownRef.current = false;
      if (closeOnOverlayClick && startedOnOverlay && event.target === event.currentTarget) {
        onClose();
      }
    },
    [closeOnOverlayClick, onClose],
  );

  if (!isOpen) {
    return null;
  }

  return createPortal(
    <div
      className="modal-overlay"
      style={{ zIndex: zIndex ?? undefined }}
      onPointerDown={onOverlayPointerDown}
      onClick={onOverlayClick}
    >
      <div
        ref={panelRef}
        className={`modal-panel modal-panel-${size}${className ? ` ${className}` : ''}`}
        style={panelStyle}
        role="dialog"
        aria-modal="true"
        aria-labelledby={`${modalId}-title`}
        tabIndex={-1}
      >
        <div className={canDrag ? 'modal-header is-draggable' : 'modal-header'} {...headerProps}>
          <h2 className="modal-title" id={`${modalId}-title`}>
            {title}
          </h2>
          <button
            type="button"
            className="modal-close"
            onClick={onClose}
            aria-label="닫기"
            data-modal-no-drag
          >
            <X size={16} aria-hidden="true" />
          </button>
        </div>

        <div className="modal-body">{children}</div>

        {footer ? <div className="modal-footer">{footer}</div> : null}

        {canResize
          ? RESIZE_HANDLES.map((handle) => (
              <span
                key={handle.dir}
                className={`modal-resize-handle modal-resize-${handle.dir}`}
                aria-hidden="true"
                {...getResizeHandleProps(handle)}
              />
            ))
          : null}
      </div>
    </div>,
    document.body,
  );
}

/**
 * Tab 순환을 패널 안에 가둔다. 모달 뒤편의 요소로 포커스가 빠져나가면 화면 낭독기 사용자가
 * 보이지 않는 컨트롤을 만지게 된다.
 */
function trapFocus(event, panel) {
  if (!panel) {
    return;
  }
  // 숨겨진 요소를 걸러낸다. offsetParent는 position: fixed 요소에서 null이 되므로
  // getClientRects()로 실제 렌더 여부를 본다.
  const focusables = Array.from(panel.querySelectorAll(FOCUSABLE_SELECTOR)).filter(
    (element) => element.getClientRects().length > 0,
  );
  if (focusables.length === 0) {
    event.preventDefault();
    panel.focus();
    return;
  }
  const first = focusables[0];
  const last = focusables[focusables.length - 1];
  if (event.shiftKey && document.activeElement === first) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && document.activeElement === last) {
    event.preventDefault();
    first.focus();
  }
}
