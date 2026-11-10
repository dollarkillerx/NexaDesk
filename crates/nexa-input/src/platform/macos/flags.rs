//! Translation between modifier sets and `CGEventFlags` bits.

use objc2_core_graphics::CGEventFlags;

use crate::hid::{Hid, ModSet, key};

pub(super) const FLAG_SHIFT: u64 = CGEventFlags::MaskShift.bits();
pub(super) const FLAG_CONTROL: u64 = CGEventFlags::MaskControl.bits();
pub(super) const FLAG_ALT: u64 = CGEventFlags::MaskAlternate.bits();
pub(super) const FLAG_COMMAND: u64 = CGEventFlags::MaskCommand.bits();
pub(super) const FLAG_ALPHA_SHIFT: u64 = CGEventFlags::MaskAlphaShift.bits();

// The device-dependent bits from IOLLEvent.h; they tell left from right.
const DEV_LCTL: u64 = 0x0000_0001;
const DEV_LSHIFT: u64 = 0x0000_0002;
const DEV_RSHIFT: u64 = 0x0000_0004;
const DEV_LCMD: u64 = 0x0000_0008;
const DEV_RCMD: u64 = 0x0000_0010;
const DEV_LALT: u64 = 0x0000_0020;
const DEV_RALT: u64 = 0x0000_0040;
const DEV_RCTL: u64 = 0x0000_2000;
const DEV_ANY: u64 =
    DEV_LCTL | DEV_LSHIFT | DEV_RSHIFT | DEV_LCMD | DEV_RCMD | DEV_LALT | DEV_RALT | DEV_RCTL;

/// `(device-independent, device-dependent)` bits of a modifier.
fn mod_bits(hid: Hid) -> Option<(u64, u64)> {
    Some(match hid {
        key::LCTRL => (FLAG_CONTROL, DEV_LCTL),
        key::RCTRL => (FLAG_CONTROL, DEV_RCTL),
        key::LSHIFT => (FLAG_SHIFT, DEV_LSHIFT),
        key::RSHIFT => (FLAG_SHIFT, DEV_RSHIFT),
        key::LALT => (FLAG_ALT, DEV_LALT),
        key::RALT => (FLAG_ALT, DEV_RALT),
        key::LGUI => (FLAG_COMMAND, DEV_LCMD),
        key::RGUI => (FLAG_COMMAND, DEV_RCMD),
        _ => return None,
    })
}

/// The flag word describing exactly `mods`.
pub(super) fn flags_for(mods: ModSet) -> u64 {
    mods.iter()
        .filter_map(mod_bits)
        .fold(0, |acc, (independent, device)| acc | independent | device)
}

/// Whether the flags say modifier `hid` is down. `None` means undecidable: the
/// modifier's class is set but the event has no device-dependent bits, as some
/// virtual keyboards send, so left and right cannot be told apart.
pub(super) fn is_down_in_flags(hid: Hid, flags: u64) -> Option<bool> {
    let (independent, device) = mod_bits(hid)?;
    if flags & independent == 0 {
        return Some(false);
    }
    if flags & DEV_ANY == 0 {
        return None;
    }
    Some(flags & device != 0)
}

/// The modifiers in `flags`, reported left-handed (see [`ModSet::collapsed`]).
pub(super) fn collapsed_mods(flags: u64) -> ModSet {
    [
        (FLAG_CONTROL, key::LCTRL),
        (FLAG_SHIFT, key::LSHIFT),
        (FLAG_ALT, key::LALT),
        (FLAG_COMMAND, key::LGUI),
    ]
    .into_iter()
    .filter(|(flag, _)| flags & flag != 0)
    .map(|(_, hid)| hid)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_for_sets_both_kinds_of_bit() {
        let flags = flags_for(ModSet::from_iter([key::LGUI, key::RSHIFT]));
        assert_eq!(flags, FLAG_COMMAND | DEV_LCMD | FLAG_SHIFT | DEV_RSHIFT);
    }

    #[test]
    fn all_eight_modifiers_have_distinct_device_bits() {
        let bits: Vec<u64> = ModSet::from_iter((0xE0..=0xE7).map(Hid::key))
            .iter()
            .map(|h| mod_bits(h).unwrap().1)
            .collect();
        assert_eq!(bits.len(), 8);
        assert_eq!(bits.iter().copied().fold(0, |a, b| a | b).count_ones(), 8);
    }

    #[test]
    fn left_and_right_are_told_apart() {
        let flags = FLAG_SHIFT | DEV_RSHIFT;
        assert_eq!(is_down_in_flags(key::RSHIFT, flags), Some(true));
        assert_eq!(is_down_in_flags(key::LSHIFT, flags), Some(false));
    }

    #[test]
    fn a_cleared_class_bit_means_released() {
        assert_eq!(is_down_in_flags(key::LCTRL, 0x100), Some(false));
        assert_eq!(
            is_down_in_flags(key::LSHIFT, FLAG_COMMAND | DEV_LCMD),
            Some(false)
        );
    }

    #[test]
    fn missing_device_bits_are_reported_as_unknown() {
        assert_eq!(is_down_in_flags(key::LSHIFT, FLAG_SHIFT), None);
    }

    #[test]
    fn collapsed_mods_ignores_noise_bits() {
        let flags = FLAG_COMMAND | FLAG_ALPHA_SHIFT | CGEventFlags::MaskNumericPad.bits();
        assert_eq!(collapsed_mods(flags), ModSet::single(key::LGUI));
    }
}
