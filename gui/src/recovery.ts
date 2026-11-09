export interface RecoveryAdvice { title: string; text: string; action: 'credentials' | 'connection' | 'mountPoint' | 'retry' }

/** 원문 오류를 유지하고, 알려진 원인에만 다음 행동을 연결합니다. */
export function recoveryAdvice(message: string): RecoveryAdvice {
  if (/UnknownIssuer/i.test(message))
    return { title: '서버 인증서 확인', text: '인증서 발급자를 신뢰할 수 없습니다. 서버 관리자에게 확인한 SHA-256 지문을 연결 정보에 입력하거나 신뢰할 수 있는 인증서를 서버에 설치하세요', action: 'connection' };
  if (/인증서|certificate|known.?hosts|호스트.?키|host key|fingerprint/i.test(message))
    return { title: '서버 신원 확인', text: '서버 관리자에게 인증서 지문 또는 호스트 키를 확인한 뒤 연결 정보를 수정하세요', action: 'connection' };
  if (/인증|암호|비밀번호|401|403|authentication|permission denied|권한/i.test(message))
    return { title: '계정과 접근 권한 확인', text: '저장된 자격 증명을 갱신하고 계정의 폴더 접근 권한을 확인하세요', action: 'credentials' };
  if (/마운트.*(사용|중복|존재)|drive.*(used|exists)|지점.*(사용|중복)|드라이브 문자/i.test(message))
    return { title: '마운트 지점 확인', text: '사용 가능한 드라이브 문자 또는 빈 폴더 경로를 지정하고 다시 연결하세요', action: 'mountPoint' };
  if (/네트워크|시간.?초과|timeout|timed out|network|dns|resolve|연결|connection/i.test(message))
    return { title: '네트워크와 서버 확인', text: '인터넷과 VPN 상태를 확인하세요. 서버 주소와 포트를 확인한 뒤 다시 검사할 수 있습니다', action: 'connection' };
  if (/찾을 수|not found|경로|404/i.test(message))
    return { title: '원격 위치 확인', text: '원격 폴더, 버킷과 접두사가 현재 서버에 있는지 확인하세요', action: 'connection' };
  return { title: '연결 다시 확인', text: '아래 원문을 확인하고 연결 검사를 다시 실행하세요', action: 'retry' };
}
