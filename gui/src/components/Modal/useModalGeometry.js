import { useCallback, useEffect, useRef, useState } from 'react';

// 모달을 아무리 끌어도 이만큼은 화면 안에 남긴다. 이 여백이 없으면 모달을 화면 밖으로
// 완전히 밀어내고 다시 잡을 수 없게 된다.
const KEEP_VISIBLE = 88;
// 헤더가 화면 위로 잘려 올라가면 손잡이를 잡을 수 없으므로 상단은 0 아래로 내려가지 않게 한다.
const MIN_TOP = 0;

// 리사이즈 핸들 방향. x/y는 "이 방향으로 끌 때 어느 변이 움직이는가"를 뜻한다.
// x: -1 왼쪽 변, +1 오른쪽 변, 0 폭 고정 / y: -1 위쪽 변, +1 아래쪽 변, 0 높이 고정
export const RESIZE_HANDLES = [
  { dir: 'n', x: 0, y: -1 },
  { dir: 's', x: 0, y: 1 },
  { dir: 'e', x: 1, y: 0 },
  { dir: 'w', x: -1, y: 0 },
  { dir: 'ne', x: 1, y: -1 },
  { dir: 'nw', x: -1, y: -1 },
  { dir: 'se', x: 1, y: 1 },
  { dir: 'sw', x: -1, y: 1 },
];

/**
 * 모달의 위치(중앙 기준 오프셋)와 크기를 관리한다.
 *
 * @param panelRef   모달 패널 엘리먼트 ref
 * @param isOpen     열림 여부. 열릴 때 저장값을 복원하고 clamp한다.
 * @param draggable  헤더 드래그 허용 여부
 * @param resizable  리사이즈 허용 여부
 * @param storageKey 세션 저장 키. 없으면 저장하지 않는다.
 * @param minWidth   최소 폭
 * @param minHeight  최소 높이
 */
export function useModalGeometry({
  panelRef,
  isOpen,
  draggable = true,
  resizable = false,
  storageKey,
  minWidth = 320,
  minHeight = 200,
}) {
  // x, y는 화면 중앙 기준 오프셋(px). width, height가 null이면 CSS 기본값(size 프리셋)을 쓴다.
  const [geometry, setGeometry] = useState({ x: 0, y: 0, width: null, height: null });
  const gestureRef = useRef(null);
  const frameRef = useRef(0);
  const pendingRef = useRef(null);

  // 터치 기기와 좁은 화면에서는 드래그와 리사이즈를 끈다. 손가락으로는 정밀 조작이 어렵고,
  // 좁은 화면에서는 모달이 거의 전체 화면이라 옮길 여지도 없다.
  const [interactive, setInteractive] = useState(true);
  useEffect(() => {
    const query = window.matchMedia('(min-width: 768px) and (pointer: fine)');
    const sync = () => setInteractive(query.matches);
    sync();
    query.addEventListener('change', sync);
    return () => query.removeEventListener('change', sync);
  }, []);

  const canDrag = draggable && interactive;
  const canResize = resizable && interactive;

  // 열릴 때마다 저장값을 복원한다. 저장 당시와 창 크기가 다를 수 있으므로(모니터 변경, 창 축소)
  // 반드시 지금 뷰포트 기준으로 clamp한 뒤 적용한다.
  useEffect(() => {
    if (!isOpen) {
      return;
    }
    const restored = storageKey ? readGeometry(storageKey) : null;
    if (!restored) {
      setGeometry({ x: 0, y: 0, width: null, height: null });
      return;
    }
    setGeometry(clampToViewport(restored, panelRef.current, { minWidth, minHeight }));
  }, [isOpen, storageKey, panelRef, minWidth, minHeight]);

  // 창 크기가 바뀌면 모달이 화면 밖에 남을 수 있으므로 다시 끌어들인다.
  useEffect(() => {
    if (!isOpen) {
      return;
    }
    const onResize = () => {
      setGeometry((current) => clampToViewport(current, panelRef.current, { minWidth, minHeight }));
    };
    window.addEventListener('resize', onResize);
    return () => window.removeEventListener('resize', onResize);
  }, [isOpen, panelRef, minWidth, minHeight]);

  const commit = useCallback(
    (next) => {
      // pointermove는 한 프레임에 여러 번 들어온다. 마지막 값만 rAF로 반영해 불필요한
      // 리렌더를 없앤다.
      pendingRef.current = next;
      if (frameRef.current) {
        return;
      }
      frameRef.current = window.requestAnimationFrame(() => {
        frameRef.current = 0;
        if (pendingRef.current) {
          setGeometry(pendingRef.current);
        }
      });
    },
    [],
  );

  const endGesture = useCallback(() => {
    const gesture = gestureRef.current;
    gestureRef.current = null;
    if (!gesture) {
      return;
    }
    if (frameRef.current) {
      window.cancelAnimationFrame(frameRef.current);
      frameRef.current = 0;
    }
    const finalGeometry = pendingRef.current ?? gesture.start;
    pendingRef.current = null;
    setGeometry(finalGeometry);
    document.body.classList.remove('modal-gesture-active');
    if (storageKey) {
      writeGeometry(storageKey, finalGeometry);
    }
  }, [storageKey]);

  const handlePointerMove = useCallback(
    (event) => {
      const gesture = gestureRef.current;
      if (!gesture) {
        return;
      }
      const dx = event.clientX - gesture.pointerX;
      const dy = event.clientY - gesture.pointerY;

      if (gesture.type === 'drag') {
        commit(
          clampToViewport(
            { ...gesture.start, x: gesture.start.x + dx, y: gesture.start.y + dy },
            panelRef.current,
            { minWidth, minHeight },
          ),
        );
        return;
      }

      // 리사이즈. 중앙 정렬 좌표계에서는 크기가 바뀌면 중앙도 함께 움직이므로, 잡고 있는
      // 변의 반대쪽 변이 제자리에 남도록 오프셋을 크기 변화량의 절반만큼 보정한다.
      const { axisX, axisY, start } = gesture;
      const baseWidth = start.width ?? gesture.measuredWidth;
      const baseHeight = start.height ?? gesture.measuredHeight;

      let width = baseWidth;
      let x = start.x;
      if (axisX !== 0) {
        width = Math.max(minWidth, Math.min(baseWidth + axisX * dx, window.innerWidth));
        x = start.x + (axisX * (width - baseWidth)) / 2;
      }

      let height = baseHeight;
      let y = start.y;
      if (axisY !== 0) {
        height = Math.max(minHeight, Math.min(baseHeight + axisY * dy, window.innerHeight));
        y = start.y + (axisY * (height - baseHeight)) / 2;
      }

      commit(clampToViewport({ x, y, width, height }, panelRef.current, { minWidth, minHeight }));
    },
    [commit, panelRef, minWidth, minHeight],
  );

  // 제스처 시작 공통 처리. setPointerCapture로 포인터를 이 요소에 묶어두면, 커서가 iframe이나
  // 다른 요소 위로 지나가도 move/up 이벤트가 끊기지 않는다.
  const beginGesture = useCallback(
    (event, gesture) => {
      const panel = panelRef.current;
      if (!panel) {
        return;
      }
      const rect = panel.getBoundingClientRect();
      event.currentTarget.setPointerCapture(event.pointerId);
      gestureRef.current = {
        ...gesture,
        pointerX: event.clientX,
        pointerY: event.clientY,
        start: geometry,
        measuredWidth: rect.width,
        measuredHeight: rect.height,
      };
      pendingRef.current = geometry;
      // 드래그 중 본문 텍스트가 선택되면서 파랗게 반전되는 것을 막는다.
      document.body.classList.add('modal-gesture-active');
    },
    [geometry, panelRef],
  );

  const onHeaderPointerDown = useCallback(
    (event) => {
      // 주 버튼만 반응한다. 헤더 안의 버튼과 입력 요소는 드래그를 시작하지 않는다.
      if (!canDrag || event.button !== 0) {
        return;
      }
      if (event.target.closest('button, a, input, select, textarea, [data-modal-no-drag]')) {
        return;
      }
      beginGesture(event, { type: 'drag' });
    },
    [canDrag, beginGesture],
  );

  const onResizePointerDown = useCallback(
    (event, handle) => {
      if (!canResize || event.button !== 0) {
        return;
      }
      event.stopPropagation();
      beginGesture(event, { type: 'resize', axisX: handle.x, axisY: handle.y });
    },
    [canResize, beginGesture],
  );

  // 키보드로도 옮길 수 있어야 한다. 헤더에 포커스를 두고 방향키를 누르면 한 번에 24px씩,
  // Shift와 함께 누르면 8px씩 움직인다.
  const onHeaderKeyDown = useCallback(
    (event) => {
      if (!canDrag) {
        return;
      }
      const step = event.shiftKey ? 8 : 24;
      const moves = {
        ArrowUp: { x: 0, y: -step },
        ArrowDown: { x: 0, y: step },
        ArrowLeft: { x: -step, y: 0 },
        ArrowRight: { x: step, y: 0 },
      };
      if (event.key === 'Home') {
        event.preventDefault();
        const reset = { ...geometry, x: 0, y: 0 };
        setGeometry(reset);
        if (storageKey) {
          writeGeometry(storageKey, reset);
        }
        return;
      }
      const move = moves[event.key];
      if (!move) {
        return;
      }
      event.preventDefault();
      const next = clampToViewport(
        { ...geometry, x: geometry.x + move.x, y: geometry.y + move.y },
        panelRef.current,
        { minWidth, minHeight },
      );
      setGeometry(next);
      if (storageKey) {
        writeGeometry(storageKey, next);
      }
    },
    [canDrag, geometry, panelRef, storageKey, minWidth, minHeight],
  );

  const panelStyle = {
    transform: `translate3d(${geometry.x}px, ${geometry.y}px, 0)`,
    width: geometry.width ? `${geometry.width}px` : undefined,
    height: geometry.height ? `${geometry.height}px` : undefined,
  };

  // 헤더에는 닫기 버튼이 들어 있으므로 role="button"을 주지 않는다. 버튼 안에 버튼이
  // 중첩되면 화면 낭독기가 구조를 잘못 읽는다. 대신 aria-roledescription으로 손잡이임을
  // 알리고 aria-keyshortcuts로 조작 키를 노출한다.
  const headerProps = canDrag
    ? {
        onPointerDown: onHeaderPointerDown,
        onPointerMove: handlePointerMove,
        onPointerUp: endGesture,
        onPointerCancel: endGesture,
        onKeyDown: onHeaderKeyDown,
        tabIndex: 0,
        'aria-roledescription': '이동 손잡이',
        'aria-label': '모달 이동 손잡이. 방향키로 옮기고 Home 키로 가운데에 되돌립니다.',
        'aria-keyshortcuts': 'ArrowUp ArrowDown ArrowLeft ArrowRight Home',
      }
    : {};

  const getResizeHandleProps = (handle) => ({
    onPointerDown: (event) => onResizePointerDown(event, handle),
    onPointerMove: handlePointerMove,
    onPointerUp: endGesture,
    onPointerCancel: endGesture,
  });

  return { panelStyle, headerProps, getResizeHandleProps, canDrag, canResize };
}

/**
 * 모달이 화면 밖으로 빠져나가지 않도록 오프셋을 가둔다.
 *
 * 패널은 중앙 정렬이 기준이므로, 실제 왼쪽 좌표는 (뷰포트폭 - 패널폭) / 2 + x가 된다.
 * 이 실제 좌표에 대한 제약을 x, y 범위로 바꿔 계산한다.
 */
function clampToViewport(geometry, panelElement, { minWidth, minHeight }) {
  const viewportWidth = window.innerWidth;
  const viewportHeight = window.innerHeight;
  const rect = panelElement?.getBoundingClientRect();
  const width = Math.max(minWidth, geometry.width ?? rect?.width ?? minWidth);
  const height = Math.max(minHeight, geometry.height ?? rect?.height ?? minHeight);

  const centeredLeft = (viewportWidth - width) / 2;
  const centeredTop = (viewportHeight - height) / 2;

  const minX = -(width - KEEP_VISIBLE) - centeredLeft;
  const maxX = viewportWidth - KEEP_VISIBLE - centeredLeft;
  const minY = MIN_TOP - centeredTop;
  const maxY = viewportHeight - KEEP_VISIBLE - centeredTop;

  return {
    ...geometry,
    x: Math.round(Math.min(Math.max(geometry.x, minX), maxX)),
    y: Math.round(Math.min(Math.max(geometry.y, minY), maxY)),
    width: geometry.width ? Math.round(Math.min(width, viewportWidth)) : null,
    height: geometry.height ? Math.round(Math.min(height, viewportHeight)) : null,
  };
}

// 위치 기억은 sessionStorage를 쓴다. 탭을 닫으면 초기화되어 "예전에 옮겨둔 자리에 모달이
// 이상하게 떠 있다"는 혼란이 남지 않는다. 영구 보존이 필요하면 localStorage로 바꾼다.
function readGeometry(storageKey) {
  try {
    const raw = window.sessionStorage.getItem(`modal-geometry:${storageKey}`);
    if (!raw) {
      return null;
    }
    const parsed = JSON.parse(raw);
    return Number.isFinite(parsed?.x) && Number.isFinite(parsed?.y) ? parsed : null;
  } catch {
    return null;
  }
}

function writeGeometry(storageKey, geometry) {
  try {
    window.sessionStorage.setItem(`modal-geometry:${storageKey}`, JSON.stringify(geometry));
  } catch {
    // 저장 실패는 기능에 영향을 주지 않으므로 조용히 넘어간다.
  }
}
