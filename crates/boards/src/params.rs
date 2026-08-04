//! Typed access to a peripheral's free-form config keys.
//!
//! Peripheral parameters cannot be a fixed struct: the whole point is that an
//! unrecognised `kind` may be served by a driver we have never seen. So they
//! stay a table, and drivers pull what they need out of it.
//!
//! The catch with free-form config is that a typo does nothing at all, and the
//! user is left wondering why their display is blank. So this tracks which keys
//! were actually read, and [`Params::unused`] reports the rest.

use std::cell::RefCell;
use std::collections::BTreeSet;
use toml::Value;

/// Why a config value could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamError {
    Missing { key: String },
    WrongType { key: String, want: &'static str, got: &'static str },
    OutOfRange { key: String, value: i64, want: &'static str },
}

impl std::fmt::Display for ParamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParamError::Missing { key } => write!(f, "missing required key {key:?}"),
            ParamError::WrongType { key, want, got } => {
                write!(f, "key {key:?} should be {want}, found {got}")
            }
            ParamError::OutOfRange { key, value, want } => {
                write!(f, "key {key:?} is {value}, which is outside {want}")
            }
        }
    }
}

impl std::error::Error for ParamError {}

pub type ParamResult<T> = Result<T, ParamError>;

/// A peripheral's config keys, tracking which have been read.
#[derive(Debug, Clone)]
pub struct Params {
    table: toml::Table,
    read: RefCell<BTreeSet<String>>,
}

impl Params {
    pub fn new(table: toml::Table) -> Self {
        Params {
            table,
            read: RefCell::new(BTreeSet::new()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    pub fn contains(&self, key: &str) -> bool {
        self.table.contains_key(key)
    }

    fn get(&self, key: &str) -> Option<&Value> {
        self.read.borrow_mut().insert(key.to_string());
        self.table.get(key)
    }

    /// Keys nobody asked for. Usually a typo, occasionally a note to a human;
    /// either way the caller should surface it rather than swallow it.
    pub fn unused(&self) -> Vec<String> {
        let read = self.read.borrow();
        self.table
            .keys()
            .filter(|k| !read.contains(*k))
            .cloned()
            .collect()
    }

    pub fn integer(&self, key: &str) -> ParamResult<i64> {
        match self.get(key) {
            None => Err(ParamError::Missing { key: key.into() }),
            Some(Value::Integer(v)) => Ok(*v),
            Some(other) => Err(ParamError::WrongType {
                key: key.into(),
                want: "an integer",
                got: other.type_str(),
            }),
        }
    }

    /// A GPIO number or bus address. Rejects negatives with a clear message
    /// rather than wrapping, since `-1` is a common way to mean "not connected"
    /// and should be spelled by omitting the key.
    pub fn u8(&self, key: &str) -> ParamResult<u8> {
        let v = self.integer(key)?;
        u8::try_from(v).map_err(|_| ParamError::OutOfRange {
            key: key.into(),
            value: v,
            want: "0..=255 (omit the key entirely for 'not connected')",
        })
    }

    pub fn u16(&self, key: &str) -> ParamResult<u16> {
        let v = self.integer(key)?;
        u16::try_from(v).map_err(|_| ParamError::OutOfRange {
            key: key.into(),
            value: v,
            want: "0..=65535",
        })
    }

    pub fn u32(&self, key: &str) -> ParamResult<u32> {
        let v = self.integer(key)?;
        u32::try_from(v).map_err(|_| ParamError::OutOfRange {
            key: key.into(),
            value: v,
            want: "0..=4294967295",
        })
    }

    pub fn string(&self, key: &str) -> ParamResult<String> {
        match self.get(key) {
            None => Err(ParamError::Missing { key: key.into() }),
            Some(Value::String(s)) => Ok(s.clone()),
            Some(other) => Err(ParamError::WrongType {
                key: key.into(),
                want: "a string",
                got: other.type_str(),
            }),
        }
    }

    pub fn bool(&self, key: &str) -> ParamResult<bool> {
        match self.get(key) {
            None => Err(ParamError::Missing { key: key.into() }),
            Some(Value::Boolean(b)) => Ok(*b),
            Some(other) => Err(ParamError::WrongType {
                key: key.into(),
                want: "a boolean",
                got: other.type_str(),
            }),
        }
    }

    /// Read a key, or fall back. Still counts as read, so an optional key is
    /// not reported as a typo.
    pub fn u8_or(&self, key: &str, default: u8) -> ParamResult<u8> {
        match self.u8(key) {
            Err(ParamError::Missing { .. }) => Ok(default),
            other => other,
        }
    }

    pub fn u16_or(&self, key: &str, default: u16) -> ParamResult<u16> {
        match self.u16(key) {
            Err(ParamError::Missing { .. }) => Ok(default),
            other => other,
        }
    }

    pub fn bool_or(&self, key: &str, default: bool) -> ParamResult<bool> {
        match self.bool(key) {
            Err(ParamError::Missing { .. }) => Ok(default),
            other => other,
        }
    }

    pub fn string_or(&self, key: &str, default: &str) -> ParamResult<String> {
        match self.string(key) {
            Err(ParamError::Missing { .. }) => Ok(default.to_string()),
            other => other,
        }
    }

    /// An optional key: absent is `None`, present-but-wrong is still an error.
    pub fn opt_u8(&self, key: &str) -> ParamResult<Option<u8>> {
        match self.u8(key) {
            Err(ParamError::Missing { .. }) => Ok(None),
            Ok(v) => Ok(Some(v)),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(src: &str) -> Params {
        Params::new(src.parse::<toml::Table>().expect("valid toml"))
    }

    #[test]
    fn reads_typed_values() {
        let p = params("cs = 12\nlabel = \"screen\"\ninvert = true");
        assert_eq!(p.u8("cs").unwrap(), 12);
        assert_eq!(p.string("label").unwrap(), "screen");
        assert!(p.bool("invert").unwrap());
    }

    #[test]
    fn missing_key_is_distinct_from_wrong_type() {
        let p = params("cs = \"twelve\"");
        assert!(matches!(p.u8("dc"), Err(ParamError::Missing { .. })));
        assert!(matches!(p.u8("cs"), Err(ParamError::WrongType { .. })));
    }

    #[test]
    fn negative_pin_is_rejected_with_advice() {
        let p = params("rst = -1");
        let err = p.u8("rst").unwrap_err();
        assert_eq!(
            err.to_string(),
            "key \"rst\" is -1, which is outside 0..=255 (omit the key entirely for 'not connected')"
        );
    }

    #[test]
    fn out_of_range_pin_is_rejected() {
        let p = params("cs = 300");
        assert!(matches!(p.u8("cs"), Err(ParamError::OutOfRange { .. })));
    }

    #[test]
    fn unread_keys_are_reported_so_typos_surface() {
        let p = params("cs = 12\nwdith = 320\nheight = 240");
        let _ = p.u8("cs");
        let _ = p.u16("width"); // the real key, misspelled in the config
        let _ = p.u16("height");
        assert_eq!(p.unused(), vec!["wdith".to_string()]);
    }

    #[test]
    fn defaulted_keys_do_not_look_like_typos() {
        let p = params("cs = 12");
        assert_eq!(p.u16_or("rotation", 0).unwrap(), 0);
        let _ = p.u8("cs");
        assert!(p.unused().is_empty());
    }

    #[test]
    fn optional_key_absent_is_none_but_malformed_still_errors() {
        let p = params("irq = \"nope\"");
        assert_eq!(params("").opt_u8("irq").unwrap(), None);
        assert!(p.opt_u8("irq").is_err());
    }
}
