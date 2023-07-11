use std::fmt;
use std::str::FromStr;

// use derive_more::Display;
use serde::de::{self, Unexpected, Visitor};
use serde::{Deserialize, Serialize};


#[derive(Default, PartialEq, Eq, Copy, Clone)]
pub struct Res {
    pub w: u32,
    pub h: u32,
}

impl fmt::Debug for Res {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.w, self.h)
    }
}

// Just allow panics because this should only ever be used to convert to/from formats that use
// signed but non-negative widths/heights.
#[allow(clippy::fallible_impl_from)]
impl From<(i32, i32)> for Res {
    fn from(wh: (i32, i32)) -> Self {
        assert!(wh.0 >= 0 && wh.1 >= 0, "Can't have negative width or height");

        Self { w: wh.0 as u32, h: wh.1 as u32 }
    }
}

impl From<(u32, u32)> for Res {
    fn from(wh: (u32, u32)) -> Self {
        Self { w: wh.0, h: wh.1 }
    }
}

impl FromStr for Res {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let split = s.split_once('x');
        if let Some((a, b)) = split {
            let a = a.parse::<u32>();
            let b = b.parse::<u32>();
            if let (Ok(w), Ok(h)) = (a, b) {
                return Ok((w, h).into());
            }
        }
        Err("Expected a string of the form WIDTHxHEIGHT. Example: 3840x2160")
    }
}

impl Res {
    pub const fn is_zero_area(self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub const fn is_zero(self) -> bool {
        self.w == 0 && self.h == 0
    }
}


struct ResVisitor;

impl<'de> Visitor<'de> for ResVisitor {
    type Value = Res;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "a string of the form WIDTHxHEIGHT. Example: 3840x2160")
    }

    fn visit_borrowed_str<E>(self, s: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        match Res::from_str(s) {
            Ok(r) => Ok(r),
            Err(_) => Err(de::Error::invalid_value(Unexpected::Str(s), &self)),
        }
    }
}

impl<'de> Deserialize<'de> for Res {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_str(ResVisitor {})
    }
}

impl Serialize for Res {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&format!("{}x{}", self.w, self.h))
    }
}
