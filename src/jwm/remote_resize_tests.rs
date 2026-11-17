// Included by input_handler tests to use its recording native backend.
fn resize_fixture() -> (Jwm, ConfigureReplyBackend, crate::core::models::ClientKey) {
    let mut backend = ConfigureReplyBackend::new();
    let mut jwm = Jwm::new_with_runtime_backend(&mut backend, "test").unwrap();
    jwm.runtime_backend = "x11rb".into();
    backend.property_ops.resize_challenge = Some([7; 32]);
    let mon = jwm.state.monitor_order[0];
    let key = add_floating_configure_client(&mut jwm, WindowId::from_raw(123), mon, 0, false);
    let c = &jwm.state.clients[key];
    *backend.window_ops.resize_geometry.lock().unwrap() = Some(Geometry {
        x: c.geometry.x,
        y: c.geometry.y,
        w: 640,
        h: 480,
        border: 0,
    });
    (jwm, backend, key)
}
fn resize_grant(jwm: &mut Jwm, backend: &mut ConfigureReplyBackend) -> u64 {
    let challenge = [7u8; 32];
    let response = jwm.remote_resize_command(
        backend,
        10,
        "remote_resize_grant",
        &serde_json::json!({"window":123,"challenge":challenge}),
    );
    assert!(response.success, "{:?}", response);
    response.data.unwrap()["grant"].as_u64().unwrap()
}
fn resize_apply(
    jwm: &mut Jwm,
    backend: &mut ConfigureReplyBackend,
    owner: u64,
    grant: u64,
) -> crate::ipc::IpcResponse {
    jwm.remote_resize_command(backend,owner,"remote_resize_apply",&serde_json::json!({"grant":grant,"expected_width":640,"expected_height":480,"width":810,"height":610}))
}
#[test]
fn remote_resize_core_calls_native_target_without_changing_focus_and_applies_hints() {
    let (mut jwm, mut backend, key) = resize_fixture();
    let focus = jwm.get_selected_client_key();
    jwm.state.clients[key].size_hints.hints_valid = true;
    jwm.state.clients[key].size_hints.inc_w = 16;
    jwm.state.clients[key].size_hints.inc_h = 16;
    let grant = resize_grant(&mut jwm, &mut backend);
    let response = resize_apply(&mut jwm, &mut backend, 10, grant);
    assert!(response.success, "{:?}", response);
    let data = response.data.unwrap();
    assert_eq!(data["status"], "configured");
    assert_eq!(data["width"], 800);
    assert_eq!(data["height"], 608);
    assert_eq!(data["constrained"], true);
    assert_eq!(jwm.get_selected_client_key(), focus);
    assert!(!jwm.suppress_layout_animation);
    let replies = backend.window_ops.replies.lock().unwrap();
    let last = replies.last().unwrap();
    assert_eq!(last.window, WindowId::from_raw(123));
    assert_eq!((last.width, last.height), (800, 608));
}
#[test]
fn remote_resize_owner_binding_unknown_fields_and_batch_bypass() {
    let (mut jwm, mut backend, _) = resize_fixture();
    let grant = resize_grant(&mut jwm, &mut backend);
    assert!(!resize_apply(&mut jwm, &mut backend, 11, grant).success);
    assert!(!jwm.remote_resize_command(&mut backend,10,"remote_resize_apply",&serde_json::json!({"grant":grant,"owner":10,"expected_width":640,"expected_height":480,"width":810,"height":610})).success);
    assert!(
        !jwm.handle_ipc_command(
            &mut backend,
            "remote_resize_apply",
            &serde_json::json!({"grant":grant})
        )
        .success
    );
    assert!(!jwm.handle_ipc_command(&mut backend,"command_batch",&serde_json::json!({"commands":[{"command":"remote_resize_apply","args":{"grant":grant,"expected_width":640,"expected_height":480,"width":810,"height":610}}]})).success);
    assert!(resize_apply(&mut jwm, &mut backend, 10, grant).success);
}
#[test]
fn remote_resize_disconnected_owner_and_reused_xid_do_not_restore_grant() {
    let (mut jwm, mut backend, key) = resize_fixture();
    let grant = resize_grant(&mut jwm, &mut backend);
    jwm.state.clients.remove(key);
    let replacement = jwm
        .state
        .clients
        .insert(WMClient::new(WindowId::from_raw(123)));
    assert_ne!(key, replacement);
    assert!(!resize_apply(&mut jwm, &mut backend, 10, grant).success);
    let (mut jwm, mut backend, _) = resize_fixture();
    let grant = resize_grant(&mut jwm, &mut backend);
    jwm.remote_resize_grants.retain_live(|_| false);
    assert!(!resize_apply(&mut jwm, &mut backend, 10, grant).success);
}
#[test]
fn remote_resize_rechecks_all_policy_boundaries_and_retires_failed_grant() {
    for case in 0..11 {
        let (mut jwm, mut backend, key) = resize_fixture();
        let grant = resize_grant(&mut jwm, &mut backend);
        match case {
            0 => backend.session_locked = true,
            1 => backend.window_ops.resize_unmapped = true,
            2 => jwm.state.clients[key].state.is_floating = false,
            3 => jwm.state.clients[key].state.is_fullscreen = true,
            4 => jwm.state.clients[key].state.is_maximized_vert = true,
            5 => jwm.state.clients[key].state.is_fixed = true,
            6 => jwm.state.clients[key].state.is_hidden = true,
            7 => jwm.state.clients[key].state.is_swallowed = true,
            8 => jwm.runtime_backend = "wayland-udev".into(),
            9 => {
                backend
                    .window_ops
                    .resize_geometry
                    .lock()
                    .unwrap()
                    .as_mut()
                    .unwrap()
                    .w = 600
            }
            _ => {
                backend.property_ops.transient =
                    Some((WindowId::from_raw(123), WindowId::from_raw(1)))
            }
        }
        assert!(
            !resize_apply(&mut jwm, &mut backend, 10, grant).success,
            "case {case}"
        );
        assert!(!resize_apply(&mut jwm, &mut backend, 10, grant).success);
    }
}
#[test]
fn remote_resize_native_error_propagates_without_mutating_geometry() {
    let (mut jwm, mut backend, key) = resize_fixture();
    let before = jwm.state.clients[key].geometry.clone();
    let grant = resize_grant(&mut jwm, &mut backend);
    backend.window_ops.resize_fail_configure = true;
    let response = resize_apply(&mut jwm, &mut backend, 10, grant);
    assert!(!response.success);
    assert!(
        response
            .error
            .unwrap()
            .contains("native window configuration failed")
    );
    assert_eq!(jwm.state.clients[key].geometry, before);
    backend.window_ops.resize_fail_configure = false;
    assert!(!resize_apply(&mut jwm, &mut backend, 10, grant).success);
}
#[test]
fn remote_resize_challenge_must_match_at_grant_and_every_apply() {
    let (mut jwm, mut backend, _) = resize_fixture();
    let challenge = [8u8; 32];
    assert!(
        !jwm.remote_resize_command(
            &mut backend,
            10,
            "remote_resize_grant",
            &serde_json::json!({"window":123,"challenge":challenge})
        )
        .success
    );
    let grant = resize_grant(&mut jwm, &mut backend);
    backend.property_ops.resize_challenge = None;
    assert!(!resize_apply(&mut jwm, &mut backend, 10, grant).success);
    backend.property_ops.resize_challenge = Some([7; 32]);
    assert!(!resize_apply(&mut jwm, &mut backend, 10, grant).success);
}
#[test]
fn remote_resize_atomic_native_position_race_cannot_move_window_back() {
    let (mut jwm, mut backend, key) = resize_fixture();
    let before = jwm.state.clients[key].geometry.clone();
    let grant = resize_grant(&mut jwm, &mut backend);
    backend.window_ops.resize_race_position = true;
    assert!(!resize_apply(&mut jwm, &mut backend, 10, grant).success);
    assert_eq!(jwm.state.clients[key].geometry, before);
}
#[test]
fn remote_resize_refuses_zero_dimensions_and_stale_size() {
    for (width, expected) in [(0, 640), (800, 600)] {
        let (mut jwm, mut backend, key) = resize_fixture();
        let before = jwm.state.clients[key].geometry.clone();
        let grant = resize_grant(&mut jwm, &mut backend);
        assert!(!jwm.remote_resize_command(&mut backend,10,"remote_resize_apply",&serde_json::json!({"grant":grant,"expected_width":expected,"expected_height":480,"width":width,"height":610})).success);
        assert_eq!(jwm.state.clients[key].geometry, before);
        assert!(!resize_apply(&mut jwm, &mut backend, 10, grant).success);
    }
}
#[test]
fn remote_resize_hints_cannot_exceed_capture_budget_before_native_submission() {
    for (width, height) in [(17000, 1000), (10000, 10000)] {
        let (mut jwm, mut backend, key) = resize_fixture();
        let before = jwm.state.clients[key].geometry.clone();
        let grant = resize_grant(&mut jwm, &mut backend);
        let hints = &mut jwm.state.clients[key].size_hints;
        hints.hints_valid = true;
        hints.min_w = width;
        hints.min_h = height;
        backend.window_ops.replies.lock().unwrap().clear();
        assert!(!resize_apply(&mut jwm, &mut backend, 10, grant).success);
        assert_eq!(jwm.state.clients[key].geometry, before);
        assert!(backend.window_ops.replies.lock().unwrap().is_empty());
    }
}
#[test]
fn remote_resize_processed_lifecycle_revokes_even_if_client_is_still_present() {
    let (mut jwm, mut backend, key) = resize_fixture();
    let grant = resize_grant(&mut jwm, &mut backend);
    jwm.remote_resize_grants.revoke_client(key);
    assert!(!resize_apply(&mut jwm, &mut backend, 10, grant).success);
}
#[test]
fn remote_resize_session_lock_transition_cannot_reactivate_grant_after_unlock() {
    let (mut jwm, mut backend, _) = resize_fixture();
    let grant = resize_grant(&mut jwm, &mut backend);
    jwm.run_session_action(&mut backend, crate::jwm::features::SessionAction::Lock)
        .unwrap();
    jwm.features.system_ui = Default::default();
    assert!(!resize_apply(&mut jwm, &mut backend, 10, grant).success);
}
