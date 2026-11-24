//! A validated RGB color for the lighting config.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// An RGB color, parsed once at the boundary from the config/UI hex form
/// `"RRGGBB"` (exactly 6 hex digits, no leading `#`).
///
/// Serializes as that hex string, so the type is drop-in TOML- and
/// wire-compatible with the raw `String` field it replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Rgb {
    r: u8,
    g: u8,
    b: u8,
}

impl Rgb {
    /// White — the lighting default.
    pub const WHITE: Self = Self::new(0xff, 0xff, 0xff);

    /// A color from its red/green/blue components.
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// The `(r, g, b)` components.
    #[must_use]
    pub const fn components(self) -> (u8, u8, u8) {
        (self.r, self.g, self.b)
    }

    /// The color packed as `0xRRGGBB` (the form GPUI's `rgb()` takes).
    #[must_use]
    pub const fn packed(self) -> u32 {
        (self.r as u32) << 16 | (self.g as u32) << 8 | self.b as u32
    }

    /// This color scaled by `percent` (0 to 100), each channel independently.
    ///
    /// No HID++ lighting feature has a device-side brightness register —
    /// "brightness" is this scaling applied client-side before the color is
    /// sent or stored. Every brightness consumer (the live write in
    /// `openlogi_agent_core::hardware::lighting_rgb`, and a stored profile's
    /// LED, which has room for one RGB triple and nothing else) must scale
    /// through here so a brightness percent means the same dimming
    /// everywhere.
    #[must_use]
    pub fn scaled_by_brightness(self, percent: u8) -> Self {
        let scale = |c: u8| u8::try_from(u16::from(c) * u16::from(percent) / 100).unwrap_or(c);
        Self::new(scale(self.r), scale(self.g), scale(self.b))
    }
}

/// A color string that is not exactly 6 hex digits.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("invalid RGB color {input:?}: expected 6 hex digits (\"RRGGBB\", no '#')")]
pub struct RgbParseError {
    /// The rejected input.
    input: String,
}

impl FromStr for Rgb {
    type Err = RgbParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let packed = (s.len() == 6)
            .then(|| u32::from_str_radix(s, 16).ok())
            .flatten()
            .ok_or_else(|| RgbParseError { input: s.into() })?;
        Ok(Self::new(
            (packed >> 16 & 0xff) as u8,
            (packed >> 8 & 0xff) as u8,
            (packed & 0xff) as u8,
        ))
    }
}

impl TryFrom<String> for Rgb {
    type Error = RgbParseError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<Rgb> for String {
    fn from(color: Rgb) -> Self {
        color.to_string()
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

#[cfg(test)]
mod tests {
    use super::Rgb;

    #[test]
    fn parses_and_round_trips_hex() {
        let color: Rgb = "8000ff".parse().unwrap();
        assert_eq!(color, Rgb::new(0x80, 0x00, 0xff));
        assert_eq!(color.packed(), 0x0080_00ff);
        assert_eq!(color.to_string(), "8000ff");
    }

    #[test]
    fn accepts_uppercase_but_prints_lowercase() {
        let color: Rgb = "FF3B30".parse().unwrap();
        assert_eq!(color.to_string(), "ff3b30");
    }

    #[test]
    fn rejects_wrong_length_prefix_and_non_hex() {
        for bad in ["fff", "ff00aa0", "#ff00aa", "red", ""] {
            assert!(bad.parse::<Rgb>().is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn scaling_by_a_hundred_percent_changes_nothing() {
        let color = Rgb::new(0x80, 0x40, 0x20);
        assert_eq!(color.scaled_by_brightness(100), color);
    }

    #[test]
    fn scaling_by_zero_percent_is_black() {
        assert_eq!(Rgb::WHITE.scaled_by_brightness(0), Rgb::new(0, 0, 0));
    }

    #[test]
    fn scaling_dims_each_channel_independently() {
        let color = Rgb::new(200, 100, 50);
        assert_eq!(color.scaled_by_brightness(50), Rgb::new(100, 50, 25));
    }
}
