/**
 * 중첩 모달의 순서와 전역 부수효과를 한곳에서 관리한다.
 *
 * 모달마다 각자 body 스크롤을 잠그면, 안쪽 모달이 닫힐 때 바깥 모달이 아직 열려 있는데도
 * 잠금이 풀린다. ESC도 마찬가지로 모든 모달이 각자 반응하면 한 번의 ESC로 여러 개가 함께
 * 닫힌다. 그래서 열린 순서를 모듈 스코프 배열 하나로 들고, 잠금은 카운트로, ESC는 맨 위
 * 모달만 반응하도록 한다.
 */
const BASE_Z_INDEX = 1000;
const Z_INDEX_STEP = 10;

const openModalIds = [];
let savedBodyOverflow = '';
let savedBodyPaddingRight = '';

export function pushModal(id) {
  if (openModalIds.length === 0) {
    lockBodyScroll();
  }
  openModalIds.push(id);
  return BASE_Z_INDEX + (openModalIds.length - 1) * Z_INDEX_STEP;
}

export function popModal(id) {
  const index = openModalIds.indexOf(id);
  if (index >= 0) {
    openModalIds.splice(index, 1);
  }
  if (openModalIds.length === 0) {
    unlockBodyScroll();
  }
}

export function isTopModal(id) {
  return openModalIds[openModalIds.length - 1] === id;
}

// 스크롤바가 사라지면서 콘텐츠가 옆으로 밀리는 현상을 막기 위해, 잠글 때 스크롤바 폭만큼
// padding-right로 보상한다.
function lockBodyScroll() {
  const scrollbarWidth = window.innerWidth - document.documentElement.clientWidth;
  savedBodyOverflow = document.body.style.overflow;
  savedBodyPaddingRight = document.body.style.paddingRight;
  document.body.style.overflow = 'hidden';
  if (scrollbarWidth > 0) {
    const currentPadding = Number.parseFloat(window.getComputedStyle(document.body).paddingRight) || 0;
    document.body.style.paddingRight = `${currentPadding + scrollbarWidth}px`;
  }
}

function unlockBodyScroll() {
  document.body.style.overflow = savedBodyOverflow;
  document.body.style.paddingRight = savedBodyPaddingRight;
  savedBodyOverflow = '';
  savedBodyPaddingRight = '';
}
