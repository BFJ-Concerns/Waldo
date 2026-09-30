//! A compiled XKB keymap reduced to the tables typing needs: for every
//! layout group, which key and which held modifiers produce each keysym.
//!
//! The keymap text is whatever the compositor reports (or, failing that,
//! what the operator's XKB configuration compiles to); nothing here guesses
//! a layout from a name. Every level a key defines is indexed, so shifted,
//! AltGr and Level5 characters resolve, provided the level's modifier mask
//! is one the modifier keys on the keymap can hold down.

use std::{collections::BTreeMap, env, ffi::OsStr};

use xkbcommon::xkb;

/// Wayland keycodes are XKB keycodes minus this offset.
pub(crate) const XKB_EVDEV_OFFSET: u32 = 8;

/// One key press with the modifier keys held around it (evdev keycodes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KeyStroke {
    pub keycode: u32,
    pub held: Vec<u32>,
}

/// The strokes that type one keysym, in order. One stroke for a direct
/// key; two for a dead key followed by a base key.
pub(crate) type KeySequence = Vec<KeyStroke>;

/// Typing tables for one layout group of the keymap.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct GroupTable {
    pub name: String,
    /// Keysym → the strokes that produce it on this group.
    pub sequences: BTreeMap<u32, KeySequence>,
    /// Keysym → a keycode carrying it at any level of this group, for
    /// key and chord actions that name a key rather than type a character.
    pub named_keycodes: BTreeMap<u32, u32>,
}

/// The keys that select levels, found by keysym on the keymap.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LevelKeys {
    pub shift: Option<u32>,
    pub level3: Option<u32>,
    pub level5: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompiledKeymap {
    /// The keymap text as uploaded to the virtual keyboard.
    pub text: String,
    pub groups: Vec<GroupTable>,
    /// Modifier key → the mask that key contributes while held.
    pub modifiers_by_keycode: BTreeMap<u32, u32>,
    /// Lock key → the mask it toggles on each press.
    pub locking_modifiers_by_keycode: BTreeMap<u32, u32>,
    pub level_keys: LevelKeys,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum KeymapError {
    /// xkbcommon could not compile the text.
    Uncompilable,
    /// The keymap defines no layout group.
    NoLayouts,
    /// The keymap has no key producing `Shift_L` or `Shift_R`, so no shifted
    /// level can be typed.
    NoShiftKey,
}

impl std::fmt::Display for KeymapError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Uncompilable => formatter.write_str("xkbcommon cannot compile the keymap"),
            Self::NoLayouts => formatter.write_str("the keymap defines no layout"),
            Self::NoShiftKey => formatter.write_str("the keymap has no Shift key"),
        }
    }
}

impl CompiledKeymap {
    /// Compiles XKB text-format keymap text into typing tables.
    pub(crate) fn compile(text: &str) -> Result<Self, KeymapError> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
        let keymap = xkb::Keymap::new_from_string(
            &context,
            text.trim_end_matches('\0').to_owned(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::COMPILE_NO_FLAGS,
        )
        .ok_or(KeymapError::Uncompilable)?;
        Self::from_keymap(&keymap, text)
    }

    /// Compiles a keymap from XKB names through a context that honours the
    /// `XKB_DEFAULT_*` environment; empty names defer to that environment.
    pub(crate) fn from_names(
        rules: &str,
        model: &str,
        layout: &str,
        variant: &str,
        options: Option<String>,
    ) -> Result<Self, KeymapError> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            rules,
            model,
            layout,
            variant,
            options,
            xkb::COMPILE_NO_FLAGS,
        )
        .ok_or(KeymapError::Uncompilable)?;
        let text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        Self::from_keymap(&keymap, &text)
    }

    fn from_keymap(keymap: &xkb::Keymap, text: &str) -> Result<Self, KeymapError> {
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .into_iter()
            .filter_map(env::var_os)
            .find(|value| !value.is_empty())
            .unwrap_or_else(|| "C".into());
        Self::from_keymap_for_locale(keymap, text, &locale)
    }

    fn from_keymap_for_locale(
        keymap: &xkb::Keymap,
        text: &str,
        locale: &OsStr,
    ) -> Result<Self, KeymapError> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
        let compose_table =
            xkb::compose::Table::new_from_locale(&context, locale, xkb::compose::COMPILE_NO_FLAGS)
                .ok();
        let mut compose = compose_table
            .as_ref()
            .map(|table| xkb::compose::State::new(table, xkb::compose::STATE_NO_FLAGS));
        let layouts = keymap.num_layouts();
        if layouts == 0 {
            return Err(KeymapError::NoLayouts);
        }
        let keycodes: Vec<xkb::Keycode> = (keymap.min_keycode().raw()..=keymap.max_keycode().raw())
            .map(xkb::Keycode::new)
            .filter(|keycode| keycode.raw() >= XKB_EVDEV_OFFSET)
            .collect();

        let level_keys = LevelKeys {
            shift: find_key(keymap, &keycodes, xkb::keysyms::KEY_Shift_L)
                .or_else(|| find_key(keymap, &keycodes, xkb::keysyms::KEY_Shift_R)),
            level3: find_key(keymap, &keycodes, xkb::keysyms::KEY_ISO_Level3_Shift),
            level5: find_key(keymap, &keycodes, xkb::keysyms::KEY_ISO_Level5_Shift),
        };
        if level_keys.shift.is_none() {
            return Err(KeymapError::NoShiftKey);
        }

        let mut modifiers_by_keycode = BTreeMap::new();
        for keysym in [
            xkb::keysyms::KEY_Shift_L,
            xkb::keysyms::KEY_Shift_R,
            xkb::keysyms::KEY_Control_L,
            xkb::keysyms::KEY_Control_R,
            xkb::keysyms::KEY_Alt_L,
            xkb::keysyms::KEY_Alt_R,
            xkb::keysyms::KEY_Super_L,
            xkb::keysyms::KEY_Super_R,
            xkb::keysyms::KEY_ISO_Level3_Shift,
            xkb::keysyms::KEY_ISO_Level5_Shift,
        ] {
            if let Some(keycode) = find_key(keymap, &keycodes, keysym) {
                let mask = held_mask(keymap, keycode);
                if mask != 0 {
                    modifiers_by_keycode.insert(keycode, mask);
                }
            }
        }
        let mut locking_modifiers_by_keycode = BTreeMap::new();
        for keysym in [xkb::keysyms::KEY_Caps_Lock, xkb::keysyms::KEY_Num_Lock] {
            if let Some(keycode) = find_key(keymap, &keycodes, keysym) {
                let mask = locked_mask(keymap, keycode);
                if mask != 0 {
                    locking_modifiers_by_keycode.insert(keycode, mask);
                }
            }
        }

        // Every modifier mask the level keys can hold down, and which keys
        // hold it: the empty combination, then each subset of the level keys.
        let level_key_masks: Vec<(u32, u32)> =
            [level_keys.shift, level_keys.level3, level_keys.level5]
                .into_iter()
                .flatten()
                .map(|keycode| {
                    (
                        keycode,
                        modifiers_by_keycode.get(&keycode).copied().unwrap_or(0),
                    )
                })
                .filter(|(_, mask)| *mask != 0)
                .collect();
        let mut realisable: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
        for subset in 0u32..(1 << level_key_masks.len()) {
            let mut mask = 0;
            let mut held = Vec::new();
            for (bit, (keycode, key_mask)) in level_key_masks.iter().enumerate() {
                if subset & (1 << bit) != 0 {
                    mask |= key_mask;
                    held.push(*keycode);
                }
            }
            realisable.entry(mask).or_insert(held);
        }
        debug_assert!(realisable.contains_key(&0));

        let mut groups = Vec::with_capacity(layouts as usize);
        for group in 0..layouts {
            let mut table = GroupTable {
                name: keymap.layout_get_name(group).to_owned(),
                ..GroupTable::default()
            };
            for keycode in &keycodes {
                let evdev = keycode.raw() - XKB_EVDEV_OFFSET;
                let levels = keymap.num_levels_for_key(*keycode, group);
                for level in 0..levels {
                    let keysyms = keymap.key_get_syms_by_level(*keycode, group, level);
                    for keysym in keysyms {
                        table.named_keycodes.entry(keysym.raw()).or_insert(evdev);
                    }
                    let mut masks = [0; 16];
                    let count = keymap
                        .key_get_mods_for_level(*keycode, group, level, &mut masks)
                        .min(masks.len());
                    let Some(held) = masks[..count]
                        .iter()
                        .filter_map(|mask| realisable.get(mask))
                        .min_by_key(|held| held.len())
                    else {
                        continue;
                    };
                    for keysym in keysyms {
                        table.sequences.entry(keysym.raw()).or_insert_with(|| {
                            vec![KeyStroke {
                                keycode: evdev,
                                held: held.clone(),
                            }]
                        });
                    }
                }
            }
            if let Some(compose) = &mut compose {
                extend_dead_keys(&mut table, compose);
            }
            groups.push(table);
        }

        Ok(Self {
            text: text.trim_end_matches('\0').to_owned(),
            groups,
            modifiers_by_keycode,
            locking_modifiers_by_keycode,
            level_keys,
        })
    }

    pub(crate) fn group(&self, index: u32) -> Option<&GroupTable> {
        self.groups.get(index as usize)
    }

    pub(crate) fn layout_names(&self) -> Vec<&str> {
        self.groups
            .iter()
            .map(|group| group.name.as_str())
            .collect()
    }
}

/// Adds only one-dead-key compositions of strokes this group can emit.
fn extend_dead_keys(table: &mut GroupTable, compose: &mut xkb::compose::State) {
    let direct = table.sequences.clone();
    for (&dead, dead_sequence) in &direct {
        // Compose recognises many starters, including Multi_key. Only dead
        // keysyms are eligible here; xkbcommon supplies their names.
        if !xkb::keysym_get_name(dead.into()).starts_with("dead_") {
            continue;
        }
        for (&base, base_sequence) in &direct {
            if xkb::keysym_get_name(base.into()).starts_with("dead_") {
                continue;
            }
            compose.reset();
            compose.feed(dead.into());
            if compose.status() != xkb::compose::Status::Composing {
                continue;
            }
            compose.feed(base.into());
            if compose.status() != xkb::compose::Status::Composed {
                continue;
            }
            let Some(text) = compose.utf8() else {
                continue;
            };
            let mut characters = text.chars();
            let Some(character) = characters.next() else {
                continue;
            };
            if characters.next().is_some() {
                continue;
            }
            table
                .sequences
                .entry(xkb::Keysym::from_char(character).raw())
                .or_insert_with(|| {
                    let mut sequence = dead_sequence.clone();
                    sequence.extend(base_sequence.iter().cloned());
                    sequence
                });
        }
    }
}

/// The evdev keycode of the first key carrying `keysym` at any level of any
/// group, or `None`.
fn find_key(keymap: &xkb::Keymap, keycodes: &[xkb::Keycode], keysym: u32) -> Option<u32> {
    keycodes.iter().find_map(|keycode| {
        (0..keymap.num_layouts_for_key(*keycode).max(1))
            .any(|group| {
                (0..keymap.num_levels_for_key(*keycode, group)).any(|level| {
                    keymap
                        .key_get_syms_by_level(*keycode, group, level)
                        .iter()
                        .any(|candidate| candidate.raw() == keysym)
                })
            })
            .then_some(keycode.raw() - XKB_EVDEV_OFFSET)
    })
}

/// The modifier mask a key contributes while held, read from a scratch
/// state rather than assumed from its name.
fn held_mask(keymap: &xkb::Keymap, evdev: u32) -> u32 {
    let keycode = xkb::Keycode::new(evdev + XKB_EVDEV_OFFSET);
    let mut state = xkb::State::new(keymap);
    state.update_key(keycode, xkb::KeyDirection::Down);
    let mask = state.serialize_mods(xkb::STATE_MODS_DEPRESSED);
    state.update_key(keycode, xkb::KeyDirection::Up);
    mask
}

/// The modifier mask a lock key toggles on a full press.
fn locked_mask(keymap: &xkb::Keymap, evdev: u32) -> u32 {
    let keycode = xkb::Keycode::new(evdev + XKB_EVDEV_OFFSET);
    let mut state = xkb::State::new(keymap);
    state.update_key(keycode, xkb::KeyDirection::Down);
    state.update_key(keycode, xkb::KeyDirection::Up);
    state.serialize_mods(xkb::STATE_MODS_LOCKED)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{CompiledKeymap, KeyStroke, KeymapError};
    use std::ffi::OsStr;
    use xkbcommon::xkb;

    /// A keymap compiled from layout names with the environment ignored,
    /// so tests are deterministic whatever the session runs.
    pub(crate) fn keymap(layouts: &str) -> CompiledKeymap {
        keymap_for_locale(layouts, "C.UTF-8")
    }

    fn keymap_for_locale(layouts: &str, locale: &str) -> CompiledKeymap {
        let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
        let compiled =
            xkb::Keymap::new_from_names(&context, "", "", layouts, "", None, xkb::COMPILE_NO_FLAGS)
                .expect("xkeyboard-config provides the test layouts");
        CompiledKeymap::from_keymap_for_locale(
            &compiled,
            &compiled.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1),
            OsStr::new(locale),
        )
        .expect("the test keymap compiles")
    }

    fn sequence(keymap: &CompiledKeymap, group: u32, character: char) -> Option<Vec<KeyStroke>> {
        keymap
            .group(group)
            .unwrap()
            .sequences
            .get(&xkb::Keysym::from_char(character).raw())
            .cloned()
    }

    #[test]
    fn dead_keys_extend_missing_characters_without_displacing_direct_strokes() {
        let intl = keymap("us(intl)");
        let table = intl.group(0).unwrap();
        assert!(
            !table
                .named_keycodes
                .contains_key(&xkb::Keysym::from_char('ê').raw()),
            "the discriminating character must have no direct stroke"
        );
        let expected = vec![
            KeyStroke {
                keycode: table.named_keycodes[&xkb::keysyms::KEY_dead_circumflex],
                held: vec![intl.level_keys.shift.unwrap()],
            },
            KeyStroke {
                keycode: table.named_keycodes[&xkb::Keysym::from_char('e').raw()],
                held: vec![],
            },
        ];
        assert_eq!(sequence(&intl, 0, 'ê'), Some(expected));
        assert_eq!(
            sequence(&intl, 0, 'é'),
            Some(vec![KeyStroke {
                keycode: table.named_keycodes[&xkb::Keysym::from_char('e').raw()],
                held: vec![intl.level_keys.level3.unwrap()],
            }])
        );
        assert_eq!(sequence(&intl, 0, '漢'), None);
        assert_eq!(sequence(&keymap("us"), 0, 'ê'), None);
    }

    #[test]
    fn unavailable_compose_data_keeps_direct_typing_available() {
        let intl = keymap_for_locale("us(intl)", "waldo_nonexistent_locale");
        assert_eq!(sequence(&intl, 0, 'ê'), None);
        let table = intl.group(0).unwrap();
        assert_eq!(
            sequence(&intl, 0, 'é'),
            Some(vec![KeyStroke {
                keycode: table.named_keycodes[&xkb::Keysym::from_char('e').raw()],
                held: vec![intl.level_keys.level3.unwrap()],
            }])
        );
        assert!(sequence(&intl, 0, 'a').is_some());
    }

    #[test]
    fn unshifted_and_shifted_levels_resolve_on_the_active_group() {
        let keymap = keymap("gb");
        let shift = keymap.level_keys.shift.unwrap();
        let w = sequence(&keymap, 0, 'w').unwrap();
        let big_w = sequence(&keymap, 0, 'W').unwrap();
        assert_eq!(w[0].held, Vec::<u32>::new());
        assert_eq!(big_w[0].held, vec![shift]);
        assert_eq!(w[0].keycode, big_w[0].keycode);
        assert_eq!(sequence(&keymap, 0, '"').unwrap()[0].held, vec![shift]);
    }

    #[test]
    fn altgr_levels_resolve_through_the_level3_key() {
        let keymap = keymap("gb");
        let level3 = keymap.level_keys.level3.expect("gb binds ISO_Level3_Shift");
        let euro = sequence(&keymap, 0, '€').expect("gb types € at AltGr+4");
        assert_eq!(euro[0].held, vec![level3]);
        let de = keymap_de();
        let at = sequence(&de, 0, '@').expect("de types @ at AltGr+q");
        assert_eq!(at[0].held, vec![de.level_keys.level3.unwrap()]);
    }

    fn keymap_de() -> CompiledKeymap {
        keymap("de")
    }

    #[test]
    fn a_character_the_layout_cannot_produce_has_no_sequence() {
        let us = keymap("us");
        assert_eq!(sequence(&us, 0, '£'), None);
        assert!(sequence(&us, 0, '$').is_some());
    }

    #[test]
    fn each_group_of_a_multi_layout_keymap_has_its_own_tables() {
        let keymap = keymap("gb,us");
        assert_eq!(keymap.layout_names(), vec!["English (UK)", "English (US)"]);
        let gb_quote = sequence(&keymap, 0, '"').unwrap();
        let us_quote = sequence(&keymap, 1, '"').unwrap();
        assert_ne!(gb_quote[0].keycode, us_quote[0].keycode);
        assert_eq!(keymap.group(2), None);
    }

    #[test]
    fn modifier_and_lock_masks_come_from_the_keymap() {
        let keymap = keymap("gb");
        let shift = keymap.level_keys.shift.unwrap();
        let level3 = keymap.level_keys.level3.unwrap();
        assert_ne!(keymap.modifiers_by_keycode[&shift], 0);
        assert_ne!(keymap.modifiers_by_keycode[&level3], 0);
        assert_ne!(
            keymap.modifiers_by_keycode[&shift],
            keymap.modifiers_by_keycode[&level3]
        );
        let caps = keymap.group(0).unwrap().named_keycodes[&xkb::keysyms::KEY_Caps_Lock];
        assert_ne!(keymap.locking_modifiers_by_keycode[&caps], 0);
    }

    #[test]
    fn text_that_is_not_a_keymap_is_reported_as_uncompilable() {
        assert_eq!(
            CompiledKeymap::compile("xkb_keymap { nonsense").unwrap_err(),
            KeymapError::Uncompilable
        );
    }
}
