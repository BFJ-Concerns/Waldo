//! The keyboard's own account of what it is holding down, and the ordered
//! transitions that carry out one prepared action from that state.

use std::collections::{BTreeMap, BTreeSet};

use waldo_protocol::{ErrorCode, InputState, ProtocolError};

use crate::keymap::KeySequence;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyboardTransition {
    Key {
        keycode: u32,
        state: InputState,
    },
    Modifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    },
}

pub(crate) enum PreparedKeyboardAction {
    Key { keycode: u32, state: InputState },
    Chord(Vec<u32>),
    Text(Vec<KeySequence>),
}

pub(crate) struct KeyboardModel {
    modifiers_by_keycode: BTreeMap<u32, u32>,
    locking_modifiers_by_keycode: BTreeMap<u32, u32>,
    depressed_modifiers: u32,
    locked_modifiers: u32,
    held_keys: BTreeSet<u32>,
    /// The layout group every modifiers event names; the compositor's
    /// active group, refreshed before each action.
    group: u32,
    /// The group the last emitted modifiers event carried, so a group
    /// change is announced before the next key.
    announced_group: Option<u32>,
}

impl KeyboardModel {
    pub(crate) fn new(
        modifiers_by_keycode: BTreeMap<u32, u32>,
        locking_modifiers_by_keycode: BTreeMap<u32, u32>,
        group: u32,
    ) -> Self {
        Self {
            modifiers_by_keycode,
            locking_modifiers_by_keycode,
            depressed_modifiers: 0,
            locked_modifiers: 0,
            held_keys: BTreeSet::new(),
            group,
            announced_group: None,
        }
    }

    pub(crate) fn set_group(&mut self, group: u32) {
        self.group = group;
    }

    pub(crate) fn has_active_modifiers(&self) -> bool {
        self.depressed_modifiers != 0 || self.locked_modifiers != 0
    }

    pub(crate) fn held_keys(&self) -> Vec<u32> {
        self.held_keys.iter().copied().collect()
    }

    pub(crate) fn clear_modifier_state(&mut self) -> Option<KeyboardTransition> {
        if !self.has_active_modifiers() {
            return None;
        }
        self.depressed_modifiers = 0;
        self.locked_modifiers = 0;
        Some(self.modifiers_transition())
    }

    pub(crate) fn plan(
        &mut self,
        prepared: PreparedKeyboardAction,
    ) -> Result<Vec<KeyboardTransition>, ProtocolError> {
        let mut transitions = Vec::new();
        if self.announced_group != Some(self.group) {
            transitions.push(self.modifiers_transition());
        }
        match prepared {
            PreparedKeyboardAction::Key { keycode, state } => {
                self.push_key(&mut transitions, keycode, state);
            }
            PreparedKeyboardAction::Chord(mut keycodes) => {
                keycodes.sort_by_key(|keycode| !self.modifiers_by_keycode.contains_key(keycode));
                for keycode in &keycodes {
                    self.push_key(&mut transitions, *keycode, InputState::Pressed);
                }
                for keycode in keycodes.iter().rev() {
                    self.push_key(&mut transitions, *keycode, InputState::Released);
                }
            }
            PreparedKeyboardAction::Text(sequences) => {
                if self.has_active_modifiers() {
                    return Err(ProtocolError {
                        code: ErrorCode::UnsupportedCapability,
                        message: "virtual-keyboard text is unavailable while an explicit modifier or lock state is active"
                            .into(),
                        retryable: false,
                    });
                }
                for stroke in sequences.into_iter().flatten() {
                    for held in &stroke.held {
                        self.push_key(&mut transitions, *held, InputState::Pressed);
                    }
                    self.push_key(&mut transitions, stroke.keycode, InputState::Pressed);
                    self.push_key(&mut transitions, stroke.keycode, InputState::Released);
                    for held in stroke.held.iter().rev() {
                        self.push_key(&mut transitions, *held, InputState::Released);
                    }
                }
            }
        }
        Ok(transitions)
    }

    fn modifiers_transition(&mut self) -> KeyboardTransition {
        self.announced_group = Some(self.group);
        KeyboardTransition::Modifiers {
            depressed: self.depressed_modifiers,
            latched: 0,
            locked: self.locked_modifiers,
            group: self.group,
        }
    }

    fn push_key(
        &mut self,
        transitions: &mut Vec<KeyboardTransition>,
        keycode: u32,
        state: InputState,
    ) {
        let was_held = self.held_keys.contains(&keycode);
        match state {
            InputState::Pressed => {
                self.held_keys.insert(keycode);
            }
            InputState::Released => {
                self.held_keys.remove(&keycode);
            }
        }
        transitions.push(KeyboardTransition::Key { keycode, state });

        let mut modifiers_changed = self.modifiers_by_keycode.contains_key(&keycode);
        if state == InputState::Pressed
            && !was_held
            && let Some(mask) = self.locking_modifiers_by_keycode.get(&keycode)
        {
            self.locked_modifiers ^= mask;
            modifiers_changed = true;
        }
        if modifiers_changed {
            self.depressed_modifiers = self
                .held_keys
                .iter()
                .filter_map(|held| self.modifiers_by_keycode.get(held))
                .fold(0, |state, mask| state | mask);
            transitions.push(self.modifiers_transition());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyboardModel, KeyboardTransition, PreparedKeyboardAction};
    use crate::keymap::{CompiledKeymap, KeyStroke, tests::keymap};
    use waldo_protocol::{ErrorCode, InputState};
    use xkbcommon::xkb;

    fn model(keymap: &CompiledKeymap, group: u32) -> KeyboardModel {
        KeyboardModel::new(
            keymap.modifiers_by_keycode.clone(),
            keymap.locking_modifiers_by_keycode.clone(),
            group,
        )
    }

    fn named(keymap: &CompiledKeymap, keysym: u32) -> u32 {
        keymap.group(0).unwrap().named_keycodes[&keysym]
    }

    fn pressed(keycode: u32) -> KeyboardTransition {
        KeyboardTransition::Key {
            keycode,
            state: InputState::Pressed,
        }
    }

    fn released(keycode: u32) -> KeyboardTransition {
        KeyboardTransition::Key {
            keycode,
            state: InputState::Released,
        }
    }

    fn modifiers(depressed: u32, locked: u32, group: u32) -> KeyboardTransition {
        KeyboardTransition::Modifiers {
            depressed,
            latched: 0,
            locked,
            group,
        }
    }

    #[test]
    fn the_active_group_is_announced_before_the_first_key() {
        let keymap = keymap("gb,us");
        let mut model = model(&keymap, 1);
        let a = named(&keymap, xkb::Keysym::from_char('a').raw());
        let transitions = model
            .plan(PreparedKeyboardAction::Key {
                keycode: a,
                state: InputState::Pressed,
            })
            .unwrap();
        assert_eq!(transitions, vec![modifiers(0, 0, 1), pressed(a)]);
        // Announced once; a later action on the same group needs no repeat.
        let again = model
            .plan(PreparedKeyboardAction::Key {
                keycode: a,
                state: InputState::Released,
            })
            .unwrap();
        assert_eq!(again, vec![released(a)]);
        model.set_group(0);
        let switched = model
            .plan(PreparedKeyboardAction::Key {
                keycode: a,
                state: InputState::Pressed,
            })
            .unwrap();
        assert_eq!(switched[0], modifiers(0, 0, 0));
    }

    #[test]
    fn a_chord_presses_modifiers_first_and_releases_in_reverse() {
        let keymap = keymap("gb");
        let mut model = model(&keymap, 0);
        model.announced_group = Some(0);
        let shift = keymap.level_keys.shift.unwrap();
        let shift_mask = keymap.modifiers_by_keycode[&shift];
        let at = named(&keymap, xkb::Keysym::from_char('@').raw());
        let transitions = model
            .plan(PreparedKeyboardAction::Chord(vec![at, shift]))
            .unwrap();
        assert_eq!(
            transitions,
            vec![
                pressed(shift),
                modifiers(shift_mask, 0, 0),
                pressed(at),
                released(at),
                released(shift),
                modifiers(0, 0, 0),
            ]
        );
        assert!(model.held_keys().is_empty());
    }

    #[test]
    fn text_holds_each_strokes_level_keys_around_the_key() {
        let keymap = keymap("gb");
        let mut model = model(&keymap, 0);
        model.announced_group = Some(0);
        let level3 = keymap.level_keys.level3.unwrap();
        let level3_mask = keymap.modifiers_by_keycode[&level3];
        let four = named(&keymap, xkb::Keysym::from_char('4').raw());
        let euro = vec![KeyStroke {
            keycode: four,
            held: vec![level3],
        }];
        let plain = vec![KeyStroke {
            keycode: four,
            held: Vec::new(),
        }];
        let transitions = model
            .plan(PreparedKeyboardAction::Text(vec![euro, plain]))
            .unwrap();
        assert_eq!(
            transitions,
            vec![
                pressed(level3),
                modifiers(level3_mask, 0, 0),
                pressed(four),
                released(four),
                released(level3),
                modifiers(0, 0, 0),
                pressed(four),
                released(four),
            ]
        );
    }

    #[test]
    fn lock_keys_toggle_on_press_and_block_text_until_cleared() {
        let keymap = keymap("gb");
        let mut model = model(&keymap, 0);
        model.announced_group = Some(0);
        let caps = named(&keymap, xkb::keysyms::KEY_Caps_Lock);
        let lock_mask = keymap.locking_modifiers_by_keycode[&caps];
        let transitions = model
            .plan(PreparedKeyboardAction::Chord(vec![caps]))
            .unwrap();
        assert_eq!(transitions[1], modifiers(0, lock_mask, 0));
        assert!(model.has_active_modifiers());
        let refused = model
            .plan(PreparedKeyboardAction::Text(Vec::new()))
            .unwrap_err();
        assert_eq!(refused.code, ErrorCode::UnsupportedCapability);
        assert_eq!(model.clear_modifier_state(), Some(modifiers(0, 0, 0)));
        assert!(!model.has_active_modifiers());
    }
}
