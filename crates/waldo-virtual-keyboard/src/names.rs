//! Key names an action document may use, resolved to XKB keysyms.

use xkbcommon::xkb;

/// Resolves a key name from an action document: a keysym name (`Return`,
/// `F5`, `a`), a single character, or one of the aliases below.
pub(crate) fn named_keysym(name: &str) -> Option<xkb::Keysym> {
    let canonical = match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" | "control_l" => "Control_L",
        "shift" | "shift_l" => "Shift_L",
        "alt" | "alt_l" => "Alt_L",
        "altgr" | "iso_level3_shift" => "ISO_Level3_Shift",
        "super" | "meta" | "super_l" => "Super_L",
        "enter" | "return" => "Return",
        "esc" | "escape" => "Escape",
        "backspace" => "BackSpace",
        "capslock" | "caps_lock" => "Caps_Lock",
        "numlock" | "num_lock" => "Num_Lock",
        "keypad1" | "numpad1" | "kp1" => "KP_1",
        "tab" => "Tab",
        "space" => "space",
        "left" => "Left",
        "right" => "Right",
        "up" => "Up",
        "down" => "Down",
        "home" => "Home",
        "end" => "End",
        "pageup" | "page_up" => "Page_Up",
        "pagedown" | "page_down" => "Page_Down",
        "delete" | "del" => "Delete",
        other if other.chars().count() == 1 => {
            return other.chars().next().map(xkb::Keysym::from_char);
        }
        _ => name,
    };
    let keysym = xkb::keysym_from_name(canonical, xkb::KEYSYM_CASE_INSENSITIVE);
    (keysym != xkb::Keysym::NoSymbol).then_some(keysym)
}

/// The keysym a text character is typed through.
pub(crate) fn text_keysym(character: char) -> xkb::Keysym {
    match character {
        '\n' | '\r' => xkb::keysyms::KEY_Return.into(),
        '\t' => xkb::keysyms::KEY_Tab.into(),
        _ => xkb::Keysym::from_char(character),
    }
}

#[cfg(test)]
mod tests {
    use super::{named_keysym, text_keysym};
    use xkbcommon::xkb;

    #[test]
    fn aliases_and_keysym_names_resolve() {
        assert_eq!(
            named_keysym("ctrl"),
            Some(xkb::keysyms::KEY_Control_L.into())
        );
        assert_eq!(
            named_keysym("Return"),
            Some(xkb::keysyms::KEY_Return.into())
        );
        assert_eq!(
            named_keysym("altgr"),
            Some(xkb::keysyms::KEY_ISO_Level3_Shift.into())
        );
        assert_eq!(named_keysym("F5"), Some(xkb::keysyms::KEY_F5.into()));
        assert_eq!(named_keysym("é"), Some(xkb::Keysym::from_char('é')));
        assert_eq!(named_keysym("definitely-not-a-key"), None);
    }

    #[test]
    fn line_breaks_and_tabs_type_through_their_keys() {
        assert_eq!(text_keysym('\n'), xkb::keysyms::KEY_Return.into());
        assert_eq!(text_keysym('\r'), xkb::keysyms::KEY_Return.into());
        assert_eq!(text_keysym('\t'), xkb::keysyms::KEY_Tab.into());
        assert_eq!(text_keysym('€'), xkb::Keysym::from_char('€'));
    }
}
