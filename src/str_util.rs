// str_util.rs

use chrono::*;

const TS_FMT_LONG: &str = "%Y-%m-%d %H:%M:%S";
const TS_FMT_SHORT: &str = "%b %d %H:%M";
const TS_FMT_SHORT_YEAR: &str = "%Y %b %d %H:%M";
const TS_NONE: &str = "(none)";

pub fn ts_fmt<S: AsRef<str>>(fmt: S, ts: i64) -> String {
    if ts == 0 {
        TS_NONE.to_string()
    } else {
        NaiveDateTime::from_timestamp_opt(ts, 0).map_or_else(
            || TS_NONE.to_string(),
            |ts| ts.format(fmt.as_ref()).to_string(),
        )
    }
}

pub trait TimeStampFormats {
    fn ts_long(self) -> String;
    fn ts_short(self) -> String;
    fn ts_short_y(self) -> String;
}
impl TimeStampFormats for i64 {
    fn ts_long(self) -> String {
        ts_fmt(TS_FMT_LONG, self)
    }

    fn ts_short(self) -> String {
        ts_fmt(TS_FMT_SHORT, self)
    }

    fn ts_short_y(self) -> String {
        ts_fmt(TS_FMT_SHORT_YEAR, self)
    }
}

// EOF
