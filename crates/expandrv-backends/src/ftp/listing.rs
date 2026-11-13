//! MLSD/MLST 사실(fact)과 LIST(Unix, DOS/IIS 형식) 줄 해석

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use expandrv_core::{Entry, EntryKind};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 현지 시각을 UTC로 보고하는 서버의 보정값. 백엔드의 모든 연결과 세션이 공유합니다.
#[derive(Default)]
pub(super) struct ServerClock {
    seconds: AtomicI64,
}

impl ServerClock {
    pub fn offset(&self) -> i64 {
        self.seconds.load(Ordering::SeqCst)
    }

    /// 방금 업로드한 파일의 원시 MLST 시각으로만 보정값을 한 번 확정합니다.
    pub fn observe_upload(&self, entry: &Entry, now: SystemTime) {
        if self.offset() != 0 {
            return;
        }
        let Some(reported) = entry.modified else {
            return;
        };
        let delta = match reported.duration_since(now) {
            Ok(d) => d.as_secs_f64(),
            Err(e) => -e.duration().as_secs_f64(),
        };
        if !(900.0..=50_400.0).contains(&delta.abs()) {
            return;
        }
        let offset = (delta / 900.0).round() as i64 * 900;
        if self
            .seconds
            .compare_exchange(0, offset, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            tracing::info!(offset_seconds = offset, "FTP 서버 시각 오프셋 확정 및 적용");
        }
    }

    pub fn apply(&self, entry: &mut Entry) {
        let offset = self.offset();
        if offset == 0 {
            return;
        }
        let duration = Duration::from_secs(offset.unsigned_abs());
        let correct = |t: SystemTime| {
            if offset > 0 {
                t.checked_sub(duration)
            } else {
                t.checked_add(duration)
            }
        };
        entry.modified = entry.modified.and_then(correct);
        entry.created = entry.created.and_then(correct);
        entry.etag = entry
            .modified
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| format!("{:x}-{:x}", d.as_secs(), entry.size));
    }
}

/// MLSD 한 줄(`fact=값;...; 이름`)을 해석합니다. `cdir`, `pdir`와 형식이 깨진 줄은 `None`입니다.
pub(super) fn parse_mlsd_line(line: &str) -> Option<Entry> {
    let (facts, name) = line.split_once(' ')?;
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    parse_facts(facts, name)
}

/// MLST 응답 줄을 해석합니다. 줄 끝 경로 대신 `name`을 항목 이름으로 씁니다.
pub(super) fn parse_mlst_line(line: &str, name: &str) -> Option<Entry> {
    let facts = line.split_once(' ').map_or(line, |(f, _)| f);
    parse_facts(facts, name)
}

fn parse_facts(facts: &str, name: &str) -> Option<Entry> {
    let mut kind = None;
    let mut size = 0;
    let mut modified = None;
    let mut created = None;
    let mut perm = None;
    for fact in facts.split(';').filter(|f| !f.is_empty()) {
        let (key, value) = fact.split_once('=')?;
        match key.to_ascii_lowercase().as_str() {
            "type" => {
                kind = Some(match value.to_ascii_lowercase().as_str() {
                    "cdir" | "pdir" => return None,
                    "dir" => EntryKind::Dir,
                    // 심볼릭 링크(OS.unix=slink 등)는 대상 종류를 알 수 없어 파일로 봅니다.
                    _ => EntryKind::File,
                });
            }
            "size" | "sizd" => size = value.parse().unwrap_or(0),
            "modify" => modified = parse_mlsd_time(value),
            "create" => created = parse_mlsd_time(value),
            "perm" => perm = Some(value.to_ascii_lowercase()),
            _ => {}
        }
    }
    let kind = kind?;
    let mut e = make_entry(name, kind, size, modified);
    e.created = created;
    // 파일의 `perm`에 쓰기(w)가 없으면 읽기 전용입니다.
    e.readonly = kind == EntryKind::File && perm.is_some_and(|p| !p.contains('w'));
    Some(e)
}

/// `YYYYMMDDHHMMSS[.sss]` UTC 시각
pub(super) fn parse_mlsd_time(s: &str) -> Option<SystemTime> {
    let (main, frac) = s.split_once('.').unwrap_or((s, ""));
    if main.len() != 14 || !main.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n = |r: std::ops::Range<usize>| main[r].parse::<u32>().ok();
    let date = NaiveDate::from_ymd_opt(n(0..4)? as i32, n(4..6)?, n(6..8)?)?;
    let time = NaiveTime::from_hms_opt(n(8..10)?, n(10..12)?, n(12..14)?)?;
    let mut nanos = 0u32;
    if !frac.is_empty() {
        if frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        nanos = frac.parse::<u32>().ok()? * 10u32.pow(9 - frac.len() as u32);
    }
    to_system_time(date.and_time(time)).map(|t| t + Duration::from_nanos(nanos.into()))
}

/// MFMT 인자용 `YYYYMMDDHHMMSS` UTC 문자열
pub(super) fn format_mfmt_time(t: SystemTime) -> String {
    DateTime::<Utc>::from(t).format("%Y%m%d%H%M%S").to_string()
}

pub(super) fn to_system_time(dt: NaiveDateTime) -> Option<SystemTime> {
    let secs = dt.and_utc().timestamp();
    if secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))
    }
}

fn make_entry(name: &str, kind: EntryKind, size: u64, modified: Option<SystemTime>) -> Entry {
    let size = if kind == EntryKind::Dir { 0 } else { size };
    let etag = modified
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| format!("{:x}-{size:x}", d.as_secs()));
    Entry {
        name: name.to_owned(),
        kind,
        size,
        modified,
        created: None,
        etag,
        readonly: false,
    }
}

/// LIST 한 줄을 해석합니다. 숫자로 시작하면 DOS/IIS 형식, 아니면 Unix `ls -l` 형식으로 봅니다.
/// `total N` 같은 머리 줄, 장치 파일, `.`과 `..`는 `None`입니다.
pub(super) fn parse_list_line(line: &str, now: DateTime<Utc>) -> Option<Entry> {
    let e = if line.starts_with(|c: char| c.is_ascii_digit()) {
        parse_dos(line)
    } else {
        parse_unix(line, now)
    }?;
    (!e.name.is_empty() && e.name != "." && e.name != "..").then_some(e)
}

/// 공백으로 구분한 토큰과 시작 바이트 위치
fn tokens(line: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in line.char_indices() {
        match (c == ' ' || c == '\t', start) {
            (true, Some(s)) => {
                out.push((s, &line[s..i]));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, &line[s..]));
    }
    out
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

fn month(tok: &str) -> Option<u32> {
    let lower = tok.to_ascii_lowercase();
    MONTHS
        .iter()
        .position(|m| *m == lower)
        .map(|i| i as u32 + 1)
}

fn hh_mm(tok: &str) -> Option<(u32, u32)> {
    let (h, m) = tok.split_once(':')?;
    let (h, m) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

fn parse_unix(line: &str, now: DateTime<Utc>) -> Option<Entry> {
    let toks = tokens(line);
    let perms = toks.first()?.1;
    if perms.len() < 10 {
        return None;
    }
    let kind = match perms.as_bytes()[0] {
        b'd' => EntryKind::Dir,
        b'-' | b'l' => EntryKind::File,
        _ => return None,
    };
    // 소유자와 그룹 열 수가 서버마다 달라 `크기 월 일 시각|연도` 묶음을 찾습니다.
    for i in 2..toks.len().saturating_sub(3) {
        let Some(mon) = month(toks[i].1) else {
            continue;
        };
        let Ok(size) = toks[i - 1].1.parse::<u64>() else {
            continue;
        };
        let Some(day) = toks[i + 1]
            .1
            .parse::<u32>()
            .ok()
            .filter(|d| (1..=31).contains(d))
        else {
            continue;
        };
        let (pos, stamp) = toks[i + 2];
        let modified = if let Some((h, m)) = hh_mm(stamp) {
            // 연도가 없으면 최근 6개월 이내 항목이므로 미래가 되면 작년으로 봅니다.
            let mut year = now.year();
            let mut dt = NaiveDate::from_ymd_opt(year, mon, day)?.and_hms_opt(h, m, 0)?;
            if dt > now.naive_utc() + chrono::Duration::days(2) {
                year -= 1;
                dt = NaiveDate::from_ymd_opt(year, mon, day)?.and_hms_opt(h, m, 0)?;
            }
            Some(dt)
        } else if stamp.len() == 4 && stamp.bytes().all(|b| b.is_ascii_digit()) {
            NaiveDate::from_ymd_opt(stamp.parse().ok()?, mon, day)?.and_hms_opt(0, 0, 0)
        } else {
            continue;
        };
        let mut name = line.get(pos + stamp.len() + 1..)?;
        if perms.starts_with('l')
            && let Some((link, _target)) = name.split_once(" -> ")
        {
            name = link;
        }
        return Some(make_entry(
            name,
            kind,
            size,
            modified.and_then(to_system_time),
        ));
    }
    None
}

/// `MM-DD-YY  HH:MMAM  <DIR>|크기  이름` (IIS, Windows 서버)
fn parse_dos(line: &str) -> Option<Entry> {
    let toks = tokens(line);
    let date = toks.first()?.1;
    let mut parts = date.split('-');
    let (mo, d, y) = (parts.next()?, parts.next()?, parts.next()?);
    let (mo, d, mut y) = (mo.parse().ok()?, d.parse().ok()?, y.parse::<i32>().ok()?);
    if y < 100 {
        y += if y < 70 { 2000 } else { 1900 };
    }
    let mut idx = 1;
    let time = toks.get(idx)?.1.to_ascii_uppercase();
    let (clock, mut meridiem) = match time.strip_suffix("AM").or(time.strip_suffix("PM")) {
        Some(c) => (c.to_owned(), Some(time.ends_with("PM"))),
        None => (time.clone(), None),
    };
    idx += 1;
    // `03:04 PM`처럼 오전/오후가 따로 오는 형식
    if meridiem.is_none()
        && let Some((_, t)) = toks.get(idx)
        && (t.eq_ignore_ascii_case("AM") || t.eq_ignore_ascii_case("PM"))
    {
        meridiem = Some(t.eq_ignore_ascii_case("PM"));
        idx += 1;
    }
    let (mut h, m) = hh_mm(&clock)?;
    if let Some(pm) = meridiem {
        h = (h % 12) + if pm { 12 } else { 0 };
    }
    let (pos, field) = *toks.get(idx)?;
    let (kind, size) = if field.eq_ignore_ascii_case("<DIR>") {
        (EntryKind::Dir, 0)
    } else {
        (EntryKind::File, field.replace(',', "").parse().ok()?)
    };
    let name = line
        .get(pos + field.len()..)?
        .trim_start_matches([' ', '\t']);
    let modified = NaiveDate::from_ymd_opt(y, mo, d)?.and_hms_opt(h, m, 0)?;
    Some(make_entry(name, kind, size, to_system_time(modified)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn server_clock_limits_and_quarter_hour_rounding() {
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        for delta in [0, 14 * 60, -14 * 60, 14 * 3600 + 1, -14 * 3600 - 1] {
            let clock = ServerClock::default();
            let mut entry = Entry::file("f", 1);
            entry.modified = Some(if delta >= 0 {
                now + Duration::from_secs(delta as u64)
            } else {
                now - Duration::from_secs((-delta) as u64)
            });
            clock.observe_upload(&entry, now);
            assert_eq!(clock.offset(), 0, "{delta}");
        }
        for (delta, expected) in [
            (900, 900),
            (-900, -900),
            (32_400, 32_400),
            (32_849, 32_400),
            (32_850, 33_300),
            (-32_850, -33_300),
            (50_400, 50_400),
            (-50_400, -50_400),
        ] {
            let clock = ServerClock::default();
            let mut entry = Entry::file("f", 1);
            entry.modified = Some(if delta >= 0 {
                now + Duration::from_secs(delta as u64)
            } else {
                now - Duration::from_secs((-delta) as u64)
            });
            clock.observe_upload(&entry, now);
            assert_eq!(clock.offset(), expected, "{delta}");
        }
    }

    #[test]
    fn server_clock_corrects_mlst_and_mlsd_without_changing_mfmt() {
        let clock = std::sync::Arc::new(ServerClock::default());
        let now = ts(2026, 11, 13, 3, 0, 0);
        let line = "type=file;size=3;modify=20261113120000;create=20261113110000;perm=rw; f";
        let mut stat = parse_mlst_line(line, "f").unwrap();
        let raw = stat.clone();
        clock.apply(&mut stat);
        assert_eq!(stat, raw);
        clock.observe_upload(&stat, now);
        assert_eq!(clock.offset(), 9 * 3600);
        clock.apply(&mut stat);
        let mut list = parse_mlsd_line(line).unwrap();
        clock.clone().apply(&mut list);
        assert_eq!(stat, list);
        assert_eq!(stat.modified, Some(now));
        assert_eq!(stat.created, Some(now - Duration::from_secs(3600)));
        assert_eq!(
            stat.etag,
            make_entry("f", EntryKind::File, 3, Some(now)).etag
        );
        assert_eq!(format_mfmt_time(now), "20261113030000");
        // 연결이 달라지거나 다음 표본이 달라도 백엔드의 확정값은 유지합니다.
        let mut next = Entry::file("g", 2);
        next.modified = Some(now + Duration::from_secs(3600));
        clock.clone().observe_upload(&next, now);
        assert_eq!(clock.offset(), 9 * 3600);
    }

    fn ts(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> SystemTime {
        to_system_time(
            NaiveDate::from_ymd_opt(y, mo, d)
                .unwrap()
                .and_hms_opt(h, mi, s)
                .unwrap(),
        )
        .unwrap()
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 15, 12, 0, 0).unwrap()
    }

    #[test]
    fn mlsd_facts() {
        let e = parse_mlsd_line(
            "type=file;size=1234;modify=20240102030405.250;perm=adfr;UNIX.mode=0644; a b.txt",
        )
        .unwrap();
        assert_eq!(
            (e.name.as_str(), e.kind, e.size),
            ("a b.txt", EntryKind::File, 1234)
        );
        assert_eq!(
            e.modified,
            Some(ts(2024, 1, 2, 3, 4, 5) + Duration::from_millis(250))
        );
        assert!(e.readonly);
        assert!(e.etag.is_some());

        let d = parse_mlsd_line("Type=dir;Modify=20231231235959;perm=flcdmpe; sub").unwrap();
        assert_eq!((d.kind, d.size), (EntryKind::Dir, 0));
        assert!(!d.readonly);

        assert!(parse_mlsd_line("type=cdir;modify=20240101000000; .").is_none());
        assert!(parse_mlsd_line("type=pdir; ..").is_none());
        assert!(parse_mlsd_line("garbage").is_none());
        // 이름이 공백으로 시작해도 첫 공백 하나만 구분자입니다.
        assert_eq!(
            parse_mlsd_line("type=file;size=1;  lead").unwrap().name,
            " lead"
        );
        let m =
            parse_mlst_line("type=file;size=9;modify=20240101000000; /x/y.bin", "y.bin").unwrap();
        assert_eq!((m.name.as_str(), m.size), ("y.bin", 9));
    }

    #[test]
    fn mlsd_time_variants() {
        assert_eq!(
            parse_mlsd_time("19691231235959"),
            Some(UNIX_EPOCH - Duration::from_secs(1))
        );
        assert!(parse_mlsd_time("2024010203040").is_none());
        assert!(parse_mlsd_time("20241302030405").is_none());
        assert_eq!(format_mfmt_time(ts(2024, 1, 2, 3, 4, 5)), "20240102030405");
    }

    #[test]
    fn unix_list_lines() {
        let e = parse_list_line(
            "-rw-r--r--    1 1000     1000      1048576 Mar 01 09:30 file with  spaces.bin",
            now(),
        )
        .unwrap();
        assert_eq!(e.name, "file with  spaces.bin");
        assert_eq!((e.kind, e.size), (EntryKind::File, 1_048_576));
        assert_eq!(e.modified, Some(ts(2026, 3, 1, 9, 30, 0)));

        // 시각 표기가 현재보다 미래면 작년 항목입니다.
        let e = parse_list_line("-rw-r--r-- 1 u g 5 Dec 24 23:00 old.txt", now()).unwrap();
        assert_eq!(e.modified, Some(ts(2025, 12, 24, 23, 0, 0)));

        let d = parse_list_line("drwxr-xr-x  2 owner group 4096 Jan  5  2019 docs", now()).unwrap();
        assert_eq!(
            (d.name.as_str(), d.kind, d.size),
            ("docs", EntryKind::Dir, 0)
        );
        assert_eq!(d.modified, Some(ts(2019, 1, 5, 0, 0, 0)));

        // 그룹 열이 없는 형식과 심볼릭 링크
        let l = parse_list_line("lrwxrwxrwx 1 ftp 11 Feb 3 2020 link -> target/x", now()).unwrap();
        assert_eq!((l.name.as_str(), l.kind), ("link", EntryKind::File));

        assert!(parse_list_line("total 12", now()).is_none());
        assert!(parse_list_line("drwxr-xr-x 2 u g 4096 Jan 01 10:00 .", now()).is_none());
        assert!(parse_list_line("crw-rw-rw- 1 u g 1, 3 Jan 01 10:00 null", now()).is_none());
    }

    #[test]
    fn dos_list_lines() {
        let d = parse_list_line("01-02-24  03:04PM       <DIR>          My Folder", now()).unwrap();
        assert_eq!((d.name.as_str(), d.kind), ("My Folder", EntryKind::Dir));
        assert_eq!(d.modified, Some(ts(2024, 1, 2, 15, 4, 0)));

        let f = parse_list_line(
            "12-31-1999  12:15AM             1,234 old report.doc",
            now(),
        )
        .unwrap();
        assert_eq!((f.name.as_str(), f.size), ("old report.doc", 1234));
        assert_eq!(f.modified, Some(ts(1999, 12, 31, 0, 15, 0)));

        let f = parse_list_line("07-04-21  13:45               77 a.txt", now()).unwrap();
        assert_eq!((f.size, f.modified), (77, Some(ts(2021, 7, 4, 13, 45, 0))));

        assert!(parse_list_line("01-02-24 garbage", now()).is_none());
    }
}
