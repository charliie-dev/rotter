use std::fmt::{self, Write};

/// Minimal JSON value; the report is small enough that a dependency is not worth it.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(u64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(&'static str, Json)>),
}

impl Json {
    pub fn get(&self, key: &str) -> &Json {
        match self {
            Json::Obj(fields) => fields
                .iter()
                .find(|(name, _)| *name == key)
                .map_or(&Json::Null, |(_, value)| value),
            _ => &Json::Null,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(value) => Some(value),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Num(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(value) => Some(*value),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> &[Json] {
        match self {
            Json::Arr(items) => items,
            _ => &[],
        }
    }

    fn write(&self, out: &mut fmt::Formatter<'_>, indent: usize) -> fmt::Result {
        match self {
            Json::Null => out.write_str("null"),
            Json::Bool(value) => write!(out, "{value}"),
            Json::Num(value) => write!(out, "{value}"),
            Json::Str(value) => write_str(out, value),
            Json::Arr(items) if items.iter().all(Json::is_scalar) => {
                out.write_char('[')?;
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.write_str(", ")?;
                    }
                    item.write(out, indent)?;
                }
                out.write_char(']')
            }
            Json::Arr(items) => {
                out.write_char('[')?;
                for (index, item) in items.iter().enumerate() {
                    out.write_str(if index > 0 { ",\n" } else { "\n" })?;
                    pad(out, indent + 1)?;
                    item.write(out, indent + 1)?;
                }
                out.write_char('\n')?;
                pad(out, indent)?;
                out.write_char(']')
            }
            Json::Obj(fields) if fields.is_empty() => out.write_str("{}"),
            Json::Obj(fields) => {
                out.write_char('{')?;
                for (index, (name, value)) in fields.iter().enumerate() {
                    out.write_str(if index > 0 { ",\n" } else { "\n" })?;
                    pad(out, indent + 1)?;
                    write_str(out, name)?;
                    out.write_str(": ")?;
                    value.write(out, indent + 1)?;
                }
                out.write_char('\n')?;
                pad(out, indent)?;
                out.write_char('}')
            }
        }
    }

    fn is_scalar(&self) -> bool {
        !matches!(self, Json::Arr(_) | Json::Obj(_))
    }
}

fn pad(out: &mut fmt::Formatter<'_>, indent: usize) -> fmt::Result {
    for _ in 0..indent {
        out.write_str("  ")?;
    }
    Ok(())
}

fn write_str(out: &mut fmt::Formatter<'_>, value: &str) -> fmt::Result {
    out.write_char('"')?;
    for character in value.chars() {
        match character {
            '"' => out.write_str("\\\"")?,
            '\\' => out.write_str("\\\\")?,
            '\n' => out.write_str("\\n")?,
            '\r' => out.write_str("\\r")?,
            '\t' => out.write_str("\\t")?,
            control if u32::from(control) < 0x20 => write!(out, "\\u{:04x}", u32::from(control))?,
            other => out.write_char(other)?,
        }
    }
    out.write_char('"')
}

impl fmt::Display for Json {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write(out, 0)
    }
}

impl From<&str> for Json {
    fn from(value: &str) -> Self {
        Json::Str(value.to_owned())
    }
}

impl From<String> for Json {
    fn from(value: String) -> Self {
        Json::Str(value)
    }
}

impl From<bool> for Json {
    fn from(value: bool) -> Self {
        Json::Bool(value)
    }
}

impl From<usize> for Json {
    fn from(value: usize) -> Self {
        Json::Num(value as u64)
    }
}

impl From<u32> for Json {
    fn from(value: u32) -> Self {
        Json::Num(value.into())
    }
}

impl<T: Into<Json>> From<Option<T>> for Json {
    fn from(value: Option<T>) -> Self {
        value.map_or(Json::Null, Into::into)
    }
}

impl<T: Into<Json>> From<Vec<T>> for Json {
    fn from(value: Vec<T>) -> Self {
        Json::Arr(value.into_iter().map(Into::into).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::Json;

    #[test]
    fn escapes_strings_and_nests() {
        let value = Json::Obj(vec![
            ("text", "a\"b\\c\nd\re\tf\u{1}é".into()),
            ("pair", vec![1usize, 2].into()),
            (
                "list",
                Json::Arr(vec![Json::Obj(vec![("ok", true.into())])]),
            ),
            ("none", Json::Null),
        ]);
        assert_eq!(
            value.to_string(),
            "{\n  \"text\": \"a\\\"b\\\\c\\nd\\re\\tf\\u0001é\",\n  \"pair\": [1, 2],\n  \"list\": [\n    {\n      \"ok\": true\n    }\n  ],\n  \"none\": null\n}"
        );
    }
}
