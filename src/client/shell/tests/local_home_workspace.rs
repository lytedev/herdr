use super::*;

fn request_home_workspace(state: &mut ClientShellState) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.begin_local_home_workspace(Ok(std::env::temp_dir()), &mut outcome);
    outcome
}

fn assert_local_home_create(actions: &[ClientShellAction], expected_boot_id: &str) {
    assert!(matches!(actions, [ClientShellAction::Endpoint { endpoint_id: ClientEndpointId::Local, boot_id, request }]
        if boot_id == expected_boot_id && matches!(&request.method,
            crate::api::schema::Method::WorkspaceCreate(params)
                if params.cwd.as_deref() == std::env::temp_dir().to_str()
                    && params.focus && params.source_workspace_id.is_none() && params.label.is_none())),
        "The shortcut must create exactly one focused workspace on Local with explicit home cwd, no source or label, and Local's boot identity; got {actions:?}");
}

#[test]
fn local_active_home_workspace_waits_for_readiness_then_creates_without_name_prompt() {
    let (mut state, _) = state_with_remote();
    state.config.prompt_new_workspace_name = true;
    let outcome = request_home_workspace(&mut state);
    assert!(
        matches!(
            outcome.actions.as_slice(),
            [ClientShellAction::ActivateEndpoint {
                endpoint_id: ClientEndpointId::Local,
                target: None
            }]
        ),
        "Even Local-active creation must pass through activation readiness, but got {:?}",
        outcome.actions
    );
    assert!(
        state.overlay.is_none(),
        "The dedicated local shortcut must never open the workspace name prompt"
    );
    let actions = state.take_local_home_workspace_actions();
    assert_local_home_create(&actions, &snapshot().boot_id);
}

#[test]
fn remote_active_home_workspace_switches_local_and_uses_local_boot_identity() {
    let (mut state, remote) = state_with_remote();
    assert!(
        state.activate_endpoint_projection(&remote),
        "The fixture's remote endpoint must be selectable"
    );
    let outcome = request_home_workspace(&mut state);
    assert!(matches!(outcome.actions.as_slice(), [ClientShellAction::ActivateEndpoint { endpoint_id: ClientEndpointId::Local, target: None }]),
        "Remote-active creation must request only Local activation, never a remote API call; got {:?}", outcome.actions);
    assert!(
        state.activate_endpoint_projection(&ClientEndpointId::Local),
        "Local activation must project the available local snapshot"
    );
    assert_eq!(
        state.active_endpoint_id,
        ClientEndpointId::Local,
        "The client must select Local before dispatching the home workspace creation"
    );
    let actions = state.take_local_home_workspace_actions();
    assert_local_home_create(&actions, &snapshot().boot_id);
}

#[test]
fn unavailable_local_home_workspace_never_creates_or_falls_back_to_remote() {
    for status in [
        ClientEndpointStatus::Reconnecting,
        ClientEndpointStatus::Disabled,
    ] {
        let (mut state, remote) = state_with_remote();
        assert!(
            state.activate_endpoint_projection(&remote),
            "The fixture remote must activate"
        );
        state.set_endpoint_status(&ClientEndpointId::Local, status);
        let outcome = request_home_workspace(&mut state);
        assert!(
            outcome.actions.is_empty(),
            "Unavailable Local must not activate or send a command to any endpoint; got {:?}",
            outcome.actions
        );
        assert!(
            state.take_local_home_workspace_actions().is_empty(),
            "Unavailable Local must not retain a creation for later"
        );
        assert_eq!(
            state.active_endpoint_id, remote,
            "Unavailable Local must leave the remote selection untouched"
        );
        assert!(
            state.visible_endpoint_notice.is_some(),
            "Unavailable Local must show the existing endpoint notice"
        );
    }
}

#[test]
fn unsupported_local_workspace_api_shows_notice_without_remote_fallback() {
    let (mut state, remote) = state_with_remote();
    state.set_endpoint_methods_for(&ClientEndpointId::Local, Some(Vec::new()));
    assert!(
        state.activate_endpoint_projection(&remote),
        "The fixture remote must activate"
    );
    request_home_workspace(&mut state);
    assert!(
        state.activate_endpoint_projection(&ClientEndpointId::Local),
        "Local must activate before its capabilities are consulted"
    );
    let actions = state.take_local_home_workspace_actions();
    assert!(
        actions.is_empty(),
        "An unsupported workspace.create API must not enqueue any command, but got {actions:?}"
    );
    assert!(
        state
            .visible_endpoint_notice
            .as_ref()
            .is_some_and(|notice| notice.key.kind == ClientEndpointNoticeKind::Unsupported),
        "Local's missing workspace.create method must show the existing unsupported-action notice"
    );
}

#[test]
fn missing_home_directory_never_uses_an_inherited_or_remote_cwd() {
    let (mut state, _) = state_with_remote();
    let mut outcome = ClientShellInput::default();
    state.begin_local_home_workspace(Err(std::io::Error::other("home missing")), &mut outcome);
    assert!(
        outcome.actions.is_empty(),
        "A missing home directory must not activate or create a workspace, but got {:?}",
        outcome.actions
    );
    assert!(
        state.visible_endpoint_notice.is_some(),
        "A missing home directory must show an unavailable notice"
    );
}

#[test]
fn local_boot_change_or_wrong_active_endpoint_cannot_release_queued_creation() {
    for change_boot in [false, true] {
        let (mut state, remote) = state_with_remote();
        request_home_workspace(&mut state);
        if change_boot {
            let mut restarted = snapshot();
            restarted.boot_id = "restarted-local".into();
            state.set_snapshot(Box::new(restarted));
        } else {
            assert!(
                state.activate_endpoint_projection(&remote),
                "The fixture remote must activate"
            );
        }
        let actions = state.take_local_home_workspace_actions();
        assert!(actions.is_empty(), "A stale boot or remote-active projection must never receive Local's queued creation; got {actions:?}");
    }
}

#[test]
fn local_active_runtime_dispatches_home_creation_only_to_local_transport() {
    use crate::client::{
        endpoint::{EndpointNegotiation, EndpointRegistry, EndpointTransport},
        endpoint_commands::EndpointCommands,
        shell_runtime::begin_endpoint_activation,
        ClientState,
    };
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<ClientMessage>>>);
    impl EndpointTransport for Capture {
        fn send(&mut self, message: &ClientMessage) -> std::io::Result<()> {
            self.0
                .lock()
                .expect("Captured requests must be accessible")
                .push(message.clone());
            Ok(())
        }
    }
    let (mut shell, remote) = state_with_remote();
    let local_sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let remote_sent = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut endpoints = EndpointRegistry::new(
        Capture(local_sent.clone()),
        1,
        EndpointNegotiation::default(),
    );
    endpoints.insert(
        remote,
        Capture(remote_sent.clone()),
        2,
        EndpointNegotiation::default(),
        false,
    );
    request_home_workspace(&mut shell);
    let mut state = ClientState::test_new();
    state.shell = Some(shell);
    begin_endpoint_activation(
        &mut state,
        &mut endpoints,
        &mut EndpointCommands::default(),
        &mut None,
        &mut 1,
        ClientEndpointId::Local,
        None,
        false,
        std::time::Instant::now(),
        &mut None,
    )
    .expect("Local-active runtime activation must succeed");
    let messages = local_sent
        .lock()
        .expect("Local captured requests must be readable");
    assert!(matches!(messages.as_slice(), [ClientMessage::ClientShellEndpointRequest { request, .. }]
        if serde_json::from_str::<crate::api::schema::Request>(request).is_ok_and(|request|
            matches!(request.method, crate::api::schema::Method::WorkspaceCreate(params)
                if params.cwd.as_deref() == std::env::temp_dir().to_str() && params.focus && params.source_workspace_id.is_none()))),
        "The activation runtime must send one explicit home workspace.create to Local, but sent {messages:?}");
    let remote_messages = remote_sent
        .lock()
        .expect("Remote captured requests must be readable");
    assert!(
        remote_messages.is_empty(),
        "Local-home creation must never reach the remote transport, but sent {remote_messages:?}"
    );
}

#[test]
fn generic_new_workspace_still_routes_to_active_remote_with_inherited_cwd() {
    let (mut state, remote) = state_with_remote();
    assert!(
        state.activate_endpoint_projection(&remote),
        "The fixture remote must activate"
    );
    state.config.prompt_new_workspace_name = false;
    let mut outcome = ClientShellInput::default();
    state.record_binding(
        crate::input::KeybindMatch::Action(crate::input::KeybindAction::NewWorkspace),
        &mut outcome,
    );
    assert!(
        matches!(outcome.actions.as_slice(), [ClientShellAction::Endpoint { endpoint_id, request, .. }]
        if endpoint_id == &remote && matches!(&request.method, crate::api::schema::Method::WorkspaceCreate(params)
            if params.cwd.is_none() && params.focus && params.source_workspace_id.as_deref() == Some("ws_1"))),
        "Generic new workspace must continue using the active remote and inherited cwd; got {:?}",
        outcome.actions
    );
}
