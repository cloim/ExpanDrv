// 공용 Modal(JS) 형식 선언. props 설명은 modal 표준 문서와 같습니다.
import type { ReactElement, ReactNode, RefObject } from 'react';

export interface ModalProps {
  isOpen: boolean;
  onClose: () => void;
  title: ReactNode;
  children: ReactNode;
  footer?: ReactNode;
  size?: 'sm' | 'md' | 'lg' | 'xl';
  draggable?: boolean;
  resizable?: boolean;
  storageKey?: string;
  closeOnOverlayClick?: boolean;
  closeOnEscape?: boolean;
  initialFocusRef?: RefObject<HTMLElement | null>;
  minWidth?: number;
  minHeight?: number;
  className?: string;
}

export function Modal(props: ModalProps): ReactElement | null;
