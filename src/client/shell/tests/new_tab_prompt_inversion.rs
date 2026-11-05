use super::*;

fn state_with_prompt(prompt_new_tab_name: bool) -> ClientShellState {
    let mut config = ClientShellConfig::from_config(&Config::default());
    config.prompt_new_tab_name = prompt_new_tab_name;
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state
}

fn press(column: u16, row: u16, modifiers: KeyModifiers) -> RawInputEvent {
    RawInputEvent::Mouse(crossterm::event::MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers,
    })
}

fn prompted(state: &ClientShellState) -> bool {
    matches!(
        state.overlay,
        Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            target: ClientRenameTarget::NewTab { .. },
            ..
        }))
    )
}

fn created_a_tab(outcome: &ClientShellInput) -> bool {
    outcome.actions.iter().any(|action| {
        matches!(
            action,
            ClientShellAction::Endpoint { request, .. }
                if matches!(request.method, crate::api::schema::Method::TabCreate(_))
        )
    })
}

/// The gesture is an inversion, not a shortcut to one behaviour: Alt-clicking must prompt when the
/// setting says don't, and skip the prompt when it says do. Testing only the configured-on case
/// would pass against an implementation that always prompts.
#[test]
fn alt_clicking_the_new_tab_button_inverts_the_prompt_either_way() {
    for prompt_new_tab_name in [false, true] {
        for modifiers in [
            KeyModifiers::empty(),
            KeyModifiers::ALT,
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        ] {
            let mut state = state_with_prompt(prompt_new_tab_name);
            state.compose(106, 20).expect("desktop shell");
            let new_tab = state.hits.new_tab;
            assert!(!new_tab.is_empty(), "the new-tab button is on screen");

            let outcome = state.handle_raw_events(vec![press(new_tab.x + 1, new_tab.y, modifiers)]);

            let should_prompt = prompt_new_tab_name != modifiers.contains(KeyModifiers::ALT);
            assert_eq!(
                prompted(&state),
                should_prompt,
                "prompt_new_tab_name={prompt_new_tab_name} modifiers={modifiers:?}"
            );
            assert_eq!(
                created_a_tab(&outcome),
                !should_prompt,
                "the un-prompted path must actually create the tab: \
                 prompt_new_tab_name={prompt_new_tab_name} modifiers={modifiers:?}"
            );
        }
    }
}

/// The mobile switcher's new-tab row is the same gesture on the other layout, and it reaches the
/// action through a different handler, so it is asserted separately rather than assumed.
#[test]
fn alt_clicking_the_mobile_new_tab_row_inverts_the_prompt_either_way() {
    for prompt_new_tab_name in [false, true] {
        for modifiers in [KeyModifiers::empty(), KeyModifiers::ALT] {
            let mut state = state_with_prompt(prompt_new_tab_name);
            state.mode = ClientShellMode::Navigate;
            state.compose(44, 20).expect("mobile switcher");
            let new_tab = state
                .hits
                .mobile_targets
                .iter()
                .find_map(|(rect, target)| {
                    matches!(target, ClientMobileTarget::NewTab).then_some(*rect)
                })
                .expect("mobile new-tab row");

            let outcome = state.handle_raw_events(vec![press(new_tab.x, new_tab.y, modifiers)]);

            let should_prompt = prompt_new_tab_name != modifiers.contains(KeyModifiers::ALT);
            assert_eq!(
                prompted(&state),
                should_prompt,
                "prompt_new_tab_name={prompt_new_tab_name} modifiers={modifiers:?}"
            );
            assert_eq!(
                created_a_tab(&outcome),
                !should_prompt,
                "prompt_new_tab_name={prompt_new_tab_name} modifiers={modifiers:?}"
            );
        }
    }
}

/// The inversion is one-shot state, and one-shot state that outlives its action is the failure
/// mode worth guarding: a stray Alt-click would silently change what a later keyboard new-tab does.
#[test]
fn an_armed_inversion_never_survives_into_a_later_action() {
    let mut state = state_with_prompt(true);
    state.compose(106, 20).expect("desktop shell");

    state.invert_new_tab_prompt = true;
    let mut outcome = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::NextTab),
        &mut outcome,
    );
    assert!(
        !state.invert_new_tab_prompt,
        "an unrelated action must consume the override"
    );

    let mut outcome = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::NewTab),
        &mut outcome,
    );
    assert!(
        prompted(&state),
        "the later new tab follows ui.prompt_new_tab_name, not the stale override"
    );
    assert!(!created_a_tab(&outcome));
}

/// Alt held over anything else on the tab row must not arm the inversion.
#[test]
fn alt_clicking_a_tab_does_not_arm_the_inversion() {
    let mut state = state_with_prompt(true);
    state.compose(106, 20).expect("desktop shell");
    let tab = state.hits.tabs[0].0;

    state.handle_raw_events(vec![press(tab.x + 1, tab.y, KeyModifiers::ALT)]);
    assert!(!state.invert_new_tab_prompt);

    let mut outcome = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::NewTab),
        &mut outcome,
    );
    assert!(prompted(&state), "the new tab still prompts");
}
