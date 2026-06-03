//! Fixed-width bit-field primitives over a `u32` word.
//!
//! The real packed layouts (definition id + flags + indices, etc.) will
//! be expressed in terms of these two functions rather than ad-hoc shifts
//! scattered across call sites — one place to get the masking right.

/// Mask covering the low `width` bits. `width == 0` -> 0, `width >= 32`
/// -> all ones (avoids the `1 << 32` overflow trap).
#[inline]
fn mask(width: u32) -> u32 {
  if width >= 32 {
    u32::MAX
  } else {
    (1u32 << width) - 1
  }
}

/// Extract the `width`-bit field starting at `offset` from `word`.
///
/// `offset + width` must be `<= 32`.
#[inline]
pub fn get_field(word: u32, offset: u32, width: u32) -> u32 {
  debug_assert!(offset + width <= 32, "field {offset}+{width} exceeds 32 bits");
  (word >> offset) & mask(width)
}

/// Return `word` with the `width`-bit field at `offset` replaced by
/// `value` (masked to `width` bits, so over-wide values can't bleed into
/// neighbouring fields).
///
/// `offset + width` must be `<= 32`.
#[inline]
pub fn set_field(word: u32, offset: u32, width: u32, value: u32) -> u32 {
  debug_assert!(offset + width <= 32, "field {offset}+{width} exceeds 32 bits");
  let m = mask(width) << offset;
  (word & !m) | ((value << offset) & m)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn round_trips() {
    let w = set_field(0, 8, 4, 0b1011);
    assert_eq!(get_field(w, 8, 4), 0b1011);
  }

  #[test]
  fn over_wide_value_is_masked() {
    // 0xFF into a 4-bit field keeps only the low 4 bits...
    let w = set_field(0, 0, 4, 0xFF);
    assert_eq!(get_field(w, 0, 4), 0xF);
    // ...and does not corrupt the adjacent field above it.
    assert_eq!(get_field(w, 4, 4), 0);
  }

  #[test]
  fn fields_are_independent() {
    let w = set_field(set_field(0, 0, 8, 0xAB), 8, 8, 0xCD);
    assert_eq!(get_field(w, 0, 8), 0xAB);
    assert_eq!(get_field(w, 8, 8), 0xCD);
  }

  #[test]
  fn full_width_mask() {
    let w = set_field(0, 0, 32, 0xDEAD_BEEF);
    assert_eq!(get_field(w, 0, 32), 0xDEAD_BEEF);
  }
}
