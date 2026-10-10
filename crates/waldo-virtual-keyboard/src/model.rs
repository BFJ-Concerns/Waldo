//! The keyboard's own account of what it is holding down, and the ordered
//! transitions that carry out one prepared action from that state.

use std::collections::{BTreeMap, BTreeSet};

use waldo_protocol::{InputState, ProtocolError};

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
        // Refuse before recording anything: a plan that fails emits no
        // transition, so it must leave no announcement behind either.
        if matches!(prepared, PreparedKeyboardAction::Text(_)) && self.has_active_modifiers() {
            return Err(ProtocolError::unsupported_capability(
                "virtual-keyboard text is unavailable while an explicit modifier or lock state is active",
            ));
        }
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

    /// An independent consumer: xkbcommon's own state machine fed the
    /// planned transitions exactly as the compositor would receive them,
    /// reporting what each non-modifier press would type.
    struct Consumer {
        state: xkb::State,
        compose: xkb::compose::State,
    }

    impl Consumer {
        fn new(keymap: &CompiledKeymap) -> Self {
            let context = xkb::Context::new(xkb::CONTEXT_NO_ENVIRONMENT_NAMES);
            let compiled = xkb::Keymap::new_from_string(
                &context,
                keymap.text.clone(),
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::COMPILE_NO_FLAGS,
            )
            .expect("the uploaded keymap text compiles for the consumer");
            let table = xkb::compose::Table::new_from_locale(
                &context,
                std::ffi::OsStr::new("C.UTF-8"),
                xkb::compose::COMPILE_NO_FLAGS,
            )
            .expect("X11 locale data provides the consumer Compose table");
            Self {
                state: xkb::State::new(&compiled),
                compose: xkb::compose::State::new(&table, xkb::compose::STATE_NO_FLAGS),
            }
        }

        /// Applies the transitions and returns the text the presses typed.
        fn consume(&mut self, transitions: &[KeyboardTransition]) -> String {
            let mut typed = String::new();
            for transition in transitions {
                match *transition {
                    KeyboardTransition::Key { keycode, state } => {
                        let xkb_keycode =
                            xkb::Keycode::new(keycode + crate::keymap::XKB_EVDEV_OFFSET);
                        if state == InputState::Pressed {
                            self.compose.feed(self.state.key_get_one_sym(xkb_keycode));
                            match self.compose.status() {
                                xkb::compose::Status::Nothing => {
                                    typed.push_str(&self.state.key_get_utf8(xkb_keycode));
                                }
                                xkb::compose::Status::Composed => {
                                    typed.push_str(&self.compose.utf8().unwrap_or_default());
                                    self.compose.reset();
                                }
                                xkb::compose::Status::Cancelled => self.compose.reset(),
                                xkb::compose::Status::Composing => {}
                            }
                        }
                        self.state.update_key(
                            xkb_keycode,
                            match state {
                                InputState::Pressed => xkb::KeyDirection::Down,
                                InputState::Released => xkb::KeyDirection::Up,
                            },
                        );
                    }
                    KeyboardTransition::Modifiers {
                        depressed,
                        latched,
                        locked,
                        group,
                    } => {
                        self.state
                            .update_mask(depressed, latched, locked, 0, 0, group);
                    }
                }
            }
            typed
        }
    }

    fn text_plan(
        keymap: &CompiledKeymap,
        group: u32,
        text: &str,
    ) -> Vec<crate::keymap::KeySequence> {
        let table = keymap.group(group).unwrap();
        text.chars()
            .map(|character| table.sequences[&crate::names::text_keysym(character).raw()].clone())
            .collect()
    }

    #[test]
    fn every_typeable_character_types_back_to_itself_through_an_xkb_consumer() {
        for layouts in ["gb", "us", "de", "fr", "us(intl)"] {
            let keymap = keymap(layouts);
            let table = keymap.group(0).unwrap();
            for (&keysym, sequence) in &table.sequences {
                let Some(character) = xkb::Keysym::new(keysym).key_char() else {
                    continue;
                };
                if character.is_control() {
                    continue;
                }
                let mut model = model(&keymap, 0);
                let transitions = model
                    .plan(PreparedKeyboardAction::Text(vec![sequence.clone()]))
                    .unwrap();
                let typed = Consumer::new(&keymap).consume(&transitions);
                assert_eq!(
                    typed,
                    character.to_string(),
                    "{layouts}: keysym {keysym:#x} planned as {transitions:?}"
                );
                assert!(
                    model.held_keys().is_empty(),
                    "{layouts}: {character:?} left keys held"
                );
            }
        }
    }

    #[test]
    fn the_announced_group_selects_which_layout_the_consumer_types_with() {
        let keymap = keymap("gb,us");
        for (group, text) in [(0, "Waldo €42! \"#"), (1, "Waldo $42! \"#")] {
            let mut model = model(&keymap, group);
            let transitions = model
                .plan(PreparedKeyboardAction::Text(text_plan(
                    &keymap, group, text,
                )))
                .unwrap();
            assert_eq!(Consumer::new(&keymap).consume(&transitions), text);
        }
    }

    #[test]
    fn a_chord_reaches_the_consumer_with_its_modifiers_active() {
        let keymap = keymap("gb");
        let mut model = model(&keymap, 0);
        let ctrl = named(&keymap, xkb::keysyms::KEY_Control_L);
        let a = named(&keymap, xkb::Keysym::from_char('a').raw());
        let transitions = model
            .plan(PreparedKeyboardAction::Chord(vec![a, ctrl]))
            .unwrap();
        let mut consumer = Consumer::new(&keymap);
        let mut control_active_at_a = None;
        for transition in &transitions {
            if let KeyboardTransition::Key {
                keycode,
                state: InputState::Pressed,
            } = transition
                && *keycode == a
            {
                control_active_at_a = Some(
                    consumer
                        .state
                        .mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE),
                );
            }
            consumer.consume(std::slice::from_ref(transition));
        }
        assert_eq!(control_active_at_a, Some(true));
        assert!(
            !consumer
                .state
                .mod_name_is_active(xkb::MOD_NAME_CTRL, xkb::STATE_MODS_EFFECTIVE)
        );
    }

    #[test]
    fn a_refused_text_plan_leaves_no_group_announcement_behind() {
        let keymap = keymap("gb,us");
        let mut model = model(&keymap, 0);
        model.announced_group = Some(0);
        let caps = named(&keymap, xkb::keysyms::KEY_Caps_Lock);
        model
            .plan(PreparedKeyboardAction::Chord(vec![caps]))
            .unwrap();
        let lock_mask = keymap.locking_modifiers_by_keycode[&caps];
        model.set_group(1);
        assert!(
            model
                .plan(PreparedKeyboardAction::Text(Vec::new()))
                .is_err()
        );
        assert_eq!(model.announced_group, Some(0));
        let a = named(&keymap, xkb::Keysym::from_char('a').raw());
        let transitions = model
            .plan(PreparedKeyboardAction::Key {
                keycode: a,
                state: InputState::Pressed,
            })
            .unwrap();
        assert_eq!(transitions[0], modifiers(0, lock_mask, 1));
    }
}
