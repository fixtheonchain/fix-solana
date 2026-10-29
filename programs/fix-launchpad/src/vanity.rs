/// Base58 digits f=38, i=41, x=55; 38*58^2+41*58+55 = 130265.
pub const FIX_SUFFIX_MODULUS: u32 = 58 * 58 * 58;
pub const FIX_SUFFIX_REMAINDER: u32 = 38 * 58 * 58 + 41 * 58 + 55;

/// A base58 suffix depends only on this big-endian integer remainder. The
/// intermediate never exceeds 49,948,671, so u32 arithmetic is sufficient.
#[inline(never)]
pub fn has_fix_suffix(bytes: &[u8]) -> bool {
    let mut remainder=0u32;
    for byte in bytes { remainder=(remainder*256+u32::from(*byte))%FIX_SUFFIX_MODULUS; }
    remainder==FIX_SUFFIX_REMAINDER
}
