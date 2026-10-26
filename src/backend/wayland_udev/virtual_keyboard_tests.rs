// Real protocol traffic against JwmWaylandState, without a renderer or fcitx5.
mod virtual_keyboard_regressions {
    use super::*;
    use smithay::backend::input::{InputTime, KeyState};
    use smithay::input::keyboard::{FilterResult, KeymapFile};
    use std::os::fd::AsRawFd;
    use xkbcommon::xkb;

    impl RawClient {
        fn vk_fd(&mut self, object: u32, args: &[u32], fd: i32) {
            let mut message = Vec::new();
            message.extend_from_slice(&object.to_ne_bytes());
            message.extend_from_slice(&(((8 + args.len() * 4) as u32) << 16).to_ne_bytes());
            for arg in args {
                message.extend_from_slice(&arg.to_ne_bytes());
            }
            let mut iov = libc::iovec {
                iov_base: message.as_mut_ptr().cast(),
                iov_len: message.len(),
            };
            // SAFETY: buffer is aligned and sized for one SCM_RIGHTS fd; all
            // pointers remain live during sendmsg and the fd is borrowed.
            unsafe {
                let mut control = [0usize; 8];
                let mut header: libc::msghdr = std::mem::zeroed();
                header.msg_iov = &mut iov;
                header.msg_iovlen = 1;
                header.msg_control = control.as_mut_ptr().cast();
                header.msg_controllen = libc::CMSG_SPACE(4) as _;
                let cmsg = libc::CMSG_FIRSTHDR(&header);
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(4) as _;
                std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<i32>(), fd);
                assert_eq!(
                    libc::sendmsg(self.peer.as_raw_fd(), &header, 0),
                    message.len() as isize
                );
            }
        }
    }

    struct App {
        client: RawClient,
        seat: u32,
        keyboard: u32,
        surface: WlSurface,
    }
    impl App {
        fn new(server: &mut Server) -> Self {
            let mut client = RawClient::connect(server, JwmClientState::default());
            let compositor = client.bind("wl_compositor", 4);
            let surface = client.new_id();
            client.request(compositor, 0, &[surface]);
            let seat = client.bind("wl_seat", 7);
            let keyboard = client.new_id();
            client.request(seat, 1, &[keyboard]);
            server.roundtrip();
            let surface = client.surface(server, surface);
            client.events();
            Self {
                client,
                seat,
                keyboard,
                surface,
            }
        }
        fn focus(&mut self, server: &mut Server) {
            server.state.set_keyboard_focus(
                Some(self.surface.clone()),
                smithay::utils::SERIAL_COUNTER.next_serial(),
            );
            server.roundtrip();
            self.client.events();
        }
        fn keys(&mut self) -> Vec<(u32, u32)> {
            self.client
                .events()
                .into_iter()
                .filter(|(id, op, _)| *id == self.keyboard && *op == 3)
                .map(|(_, _, args)| (args[2], args[3]))
                .collect()
        }
    }
    fn virtual_keyboard(server: &mut Server, client: &mut RawClient, seat: u32) -> u32 {
        let manager = client.bind("zwp_virtual_keyboard_manager_v1", 1);
        let vk = client.new_id();
        client.request(manager, 0, &[seat, vk]);
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let map = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        KeymapFile::new(&map)
            .with_fd(true, |fd, size| {
                client.vk_fd(vk, &[1, size as u32], fd.as_raw_fd())
            })
            .unwrap();
        server.roundtrip();
        client.events();
        vk
    }
    fn key(client: &mut RawClient, vk: u32, key: u32, pressed: bool) {
        client.request(vk, 1, &[42, key, u32::from(pressed)]);
    }
    fn physical(server: &mut Server, code: u32, pressed: bool) {
        server.state.keyboard_input_for_popup(
            (code + 8).into(),
            if pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            },
            smithay::utils::SERIAL_COUNTER.next_serial(),
            InputTime::from_millis(43),
            |_, _, _| FilterResult::Forward,
        );
        server.roundtrip();
    }

    #[test]
    fn virtual_keyboard_keycodes_and_balanced_delivery() {
        let mut server = Server::new();
        let mut app = App::new(&mut server);
        app.focus(&mut server);
        let mut ime = App::new(&mut server);
        let vk = virtual_keyboard(&mut server, &mut ime.client, ime.seat);
        key(&mut ime.client, vk, 30, true);
        key(&mut ime.client, vk, 30, false);
        server.roundtrip();
        assert_eq!(app.keys(), [(30, 1), (30, 0)]);
        assert!(
            server.backend_events.lock().unwrap().is_empty(),
            "virtual forwarding must not emit WM shortcuts"
        );
    }

    #[test]
    fn virtual_keyboard_focus_switch_does_not_release_into_another_client() {
        let mut server = Server::new();
        let mut a = App::new(&mut server);
        let mut b = App::new(&mut server);
        a.focus(&mut server);
        let mut ime = App::new(&mut server);
        let vk = virtual_keyboard(&mut server, &mut ime.client, ime.seat);
        key(&mut ime.client, vk, 30, true);
        server.roundtrip();
        assert_eq!(a.keys(), [(30, 1)]);
        b.focus(&mut server);
        key(&mut ime.client, vk, 30, false);
        server.roundtrip();
        assert!(b.keys().is_empty());
        key(&mut ime.client, vk, 31, true);
        key(&mut ime.client, vk, 31, false);
        server.roundtrip();
        assert_eq!(b.keys(), [(31, 1), (31, 0)]);
        assert!(a.keys().is_empty());
    }

    #[test]
    fn virtual_keyboard_modifiers_and_destroy_preserve_physical_shift() {
        let mut server = Server::new();
        let mut app = App::new(&mut server);
        app.focus(&mut server);
        physical(&mut server, 42, true);
        app.client.events();
        let keyboard = server.state.seat.get_keyboard().unwrap();
        let before = keyboard.modifier_state();
        assert!(before.shift);
        let mut ime = App::new(&mut server);
        let vk = virtual_keyboard(&mut server, &mut ime.client, ime.seat);
        ime.client.request(vk, 2, &[1, 0, 2, 0]);
        key(&mut ime.client, vk, 30, true);
        server.roundtrip();
        let events = app.client.events();
        assert!(
            events
                .iter()
                .any(|(id, op, args)| *id == app.keyboard && *op == 4 && args[1..] == [1, 0, 2, 0])
        );
        assert_eq!(keyboard.modifier_state(), before);
        ime.client.request(vk, 3, &[]);
        server.roundtrip();
        assert_eq!(app.keys(), [(30, 0)]);
        assert_eq!(keyboard.modifier_state(), before);
        assert!(server.state.virtual_keyboards.is_empty());
        physical(&mut server, 42, false);
        assert!(!keyboard.modifier_state().shift);
    }

    #[test]
    fn virtual_keyboard_lock_blocks_keys_modifiers_and_stale_releases() {
        let mut server = Server::new();
        let mut app = App::new(&mut server);
        app.focus(&mut server);
        let mut ime = App::new(&mut server);
        let vk = virtual_keyboard(&mut server, &mut ime.client, ime.seat);
        key(&mut ime.client, vk, 30, true);
        server.roundtrip();
        app.client.events();
        // Deliberately retain an app focus: even a late/stale focus must not leak.
        server.state.session_locked = true;
        server.state.session_lock_epoch += 1;
        ime.client.request(vk, 2, &[1, 0, 0, 0]);
        key(&mut ime.client, vk, 31, true);
        server.roundtrip();
        assert!(app.client.events().is_empty());
        server.state.session_locked = false;
        key(&mut ime.client, vk, 30, false);
        key(&mut ime.client, vk, 31, false);
        server.roundtrip();
        assert!(app.keys().is_empty());
        key(&mut ime.client, vk, 32, true);
        key(&mut ime.client, vk, 32, false);
        server.roundtrip();
        assert_eq!(app.keys(), [(32, 1), (32, 0)]);
    }

    #[test]
    fn virtual_keyboard_no_focus_does_not_poison_physical_state() {
        let mut server = Server::new();
        let mut ime = App::new(&mut server);
        let vk = virtual_keyboard(&mut server, &mut ime.client, ime.seat);
        ime.client.request(vk, 2, &[1, 0, 0, 0]);
        key(&mut ime.client, vk, 30, true);
        server.roundtrip();
        assert!(
            !server
                .state
                .seat
                .get_keyboard()
                .unwrap()
                .modifier_state()
                .shift
        );
        let mut app = App::new(&mut server);
        app.focus(&mut server);
        key(&mut ime.client, vk, 30, false);
        server.roundtrip();
        assert!(app.keys().is_empty());
    }
    fn input_method(server: &mut Server, ime: &mut App) -> u32 {
        let manager = ime.client.bind("zwp_input_method_manager_v2", 1);
        let im = ime.client.new_id();
        ime.client.request(manager, 0, &[ime.seat, im]);
        server.roundtrip();
        im
    }

    #[test]
    fn virtual_keyboard_ime_passthrough_does_not_reenter_grab() {
        let mut server = Server::new();
        let mut app = App::new(&mut server);
        let mut ime = App::new(&mut server);
        let vk = virtual_keyboard(&mut server, &mut ime.client, ime.seat);
        let im = input_method(&mut server, &mut ime);
        app.focus(&mut server);
        let grab = ime.client.new_id();
        ime.client.request(im, 5, &[grab]);
        server.roundtrip();
        ime.client.events();
        physical(&mut server, 30, true);
        let events = ime.client.events();
        assert_eq!(
            events
                .iter()
                .filter(|(id, op, _)| *id == grab && *op == 1)
                .count(),
            1
        );
        assert!(app.keys().is_empty());
        key(&mut ime.client, vk, 30, true);
        key(&mut ime.client, vk, 30, false);
        server.roundtrip();
        assert_eq!(app.keys(), [(30, 1), (30, 0)]);
        assert!(
            !ime.client
                .events()
                .iter()
                .any(|(id, op, _)| *id == grab && *op == 1),
            "IME forwarding loop"
        );
        physical(&mut server, 30, false);
        assert_eq!(
            ime.client
                .events()
                .iter()
                .filter(|(id, op, _)| *id == grab && *op == 1)
                .count(),
            1
        );
    }

    fn wire_string(args: &[u32]) -> String {
        let bytes: Vec<u8> = args[1..]
            .iter()
            .flat_map(|word| word.to_ne_bytes())
            .collect();
        String::from_utf8(bytes[..args[0] as usize - 1].to_vec()).unwrap()
    }

    #[test]
    fn virtual_keyboard_ime_preedit_candidate_commit_and_focus_deactivation() {
        let mut server = Server::new();
        let mut app = App::new(&mut server);
        let mut ime = App::new(&mut server);
        let _vk = virtual_keyboard(&mut server, &mut ime.client, ime.seat);
        let im = input_method(&mut server, &mut ime);
        let manager = app.client.bind("zwp_text_input_manager_v3", 1);
        let text = app.client.new_id();
        app.client.request(manager, 1, &[text, app.seat]);
        server.roundtrip();
        app.focus(&mut server);
        app.client.request(text, 1, &[]);
        app.client.request(text, 7, &[]);
        server.roundtrip();
        let serial = ime
            .client
            .events()
            .iter()
            .filter(|(id, op, _)| *id == im && *op == 5)
            .count() as u32;
        assert!(serial > 0, "text input must activate the IME");
        app.client.events();
        let mut preedit = string_words("nihao");
        preedit.extend([5, 5]);
        ime.client.request(im, 1, &preedit);
        ime.client.request(im, 3, &[serial]);
        server.roundtrip();
        let events = app.client.events();
        assert!(
            events
                .iter()
                .any(|(id, op, args)| *id == text && *op == 2 && wire_string(args) == "nihao")
        );
        assert!(
            events
                .iter()
                .any(|(id, op, args)| *id == text && *op == 5 && args[0] == 1)
        );
        // The protocol carries the candidate chosen by the IME. This tests
        // UTF-8 commit transport, not fcitx's dictionary or candidate UI.
        ime.client.request(im, 0, &string_words("你好"));
        ime.client.request(im, 3, &[serial]);
        server.roundtrip();
        assert!(
            app.client
                .events()
                .iter()
                .any(|(id, op, args)| *id == text && *op == 3 && wire_string(args) == "你好")
        );
        let mut other = App::new(&mut server);
        other.focus(&mut server);
        assert!(
            ime.client
                .events()
                .iter()
                .any(|(id, op, _)| *id == im && *op == 1)
        );
        app.client.events();
        ime.client.request(im, 0, &string_words("不应泄漏"));
        ime.client.request(im, 3, &[serial]);
        server.roundtrip();
        assert!(
            !app.client
                .events()
                .iter()
                .any(|(id, op, _)| *id == text && *op == 3)
        );
    }
    #[test]
    fn virtual_keyboard_focus_roundtrip_invalidates_old_press() {
        let mut server = Server::new();
        let mut a = App::new(&mut server);
        let mut b = App::new(&mut server);
        let mut ime = App::new(&mut server);
        let vk = virtual_keyboard(&mut server, &mut ime.client, ime.seat);
        a.focus(&mut server);
        key(&mut ime.client, vk, 30, true);
        server.roundtrip();
        a.keys();
        b.focus(&mut server);
        a.focus(&mut server);
        key(&mut ime.client, vk, 30, false);
        server.roundtrip();
        assert!(a.keys().is_empty());
    }

    #[test]
    fn virtual_keyboard_no_keymap_tracks_physical_map_reload() {
        let mut server = Server::new();
        let mut app = App::new(&mut server);
        app.focus(&mut server);
        let mut ime = App::new(&mut server);
        let vk = virtual_keyboard(&mut server, &mut ime.client, ime.seat);
        let fd = std::fs::File::open("/dev/null").unwrap();
        ime.client.vk_fd(vk, &[0, 0], fd.as_raw_fd());
        server.roundtrip();
        let keyboard = server.state.seat.get_keyboard().unwrap();
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let map = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "de",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap()
        .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        keyboard
            .set_keymap_from_string(&mut server.state, map.clone())
            .unwrap();
        key(&mut ime.client, vk, 21, true);
        key(&mut ime.client, vk, 21, false);
        server.roundtrip();
        assert_eq!(app.keys(), [(21, 1), (21, 0)]);
        let actual = keyboard.with_xkb_state(&mut server.state, |ctx| {
            ctx.xkb().lock().unwrap().keymap_as_string()
        });
        assert_eq!(actual, map);
        // A physical shortcut filter still runs, and can intercept the key.
        let mut filtered = false;
        server.state.keyboard_input_for_popup(
            29u32.into(),
            KeyState::Pressed,
            smithay::utils::SERIAL_COUNTER.next_serial(),
            InputTime::from_millis(44),
            |_, _, _| {
                filtered = true;
                FilterResult::Intercept(())
            },
        );
        assert!(filtered);
        server.roundtrip();
        assert!(app.keys().is_empty());
        physical(&mut server, 21, false);
    }

    #[test]
    fn virtual_keyboard_lock_epoch_clears_modifiers_without_locked_traffic() {
        let mut server = Server::new();
        let mut app = App::new(&mut server);
        app.focus(&mut server);
        let mut ime = App::new(&mut server);
        let vk = virtual_keyboard(&mut server, &mut ime.client, ime.seat);
        ime.client.request(vk, 2, &[1, 0, 0, 0]);
        key(&mut ime.client, vk, 30, true);
        server.roundtrip();
        app.client.events();
        server.state.session_lock_epoch += 1; // complete lock/unlock between requests
        key(&mut ime.client, vk, 30, false);
        key(&mut ime.client, vk, 31, true);
        server.roundtrip();
        let events = app.client.events();
        assert!(
            !events
                .iter()
                .any(|(id, op, args)| *id == app.keyboard && *op == 3 && args[2] == 30)
        );
        assert!(
            events
                .iter()
                .any(|(id, op, args)| *id == app.keyboard && *op == 4 && args[1..] == [0, 0, 0, 0])
        );
    }

    #[test]
    fn virtual_keyboard_key_without_keymap_is_protocol_error() {
        let mut server = Server::new();
        let mut ime = App::new(&mut server);
        let manager = ime.client.bind("zwp_virtual_keyboard_manager_v1", 1);
        let vk = ime.client.new_id();
        ime.client.request(manager, 0, &[ime.seat, vk]);
        key(&mut ime.client, vk, 30, true);
        server.roundtrip();
        assert!(
            ime.client
                .events()
                .iter()
                .any(|(id, op, args)| *id == 1 && *op == 0 && args[0] == vk)
        );
        assert!(server.state.virtual_keyboards.is_empty());
    }
    #[test]
    fn virtual_keyboard_session_lock_protocol_isolates_locker_and_apps() {
        let mut server = Server::new();
        let output = test_output("VK-LOCK");
        output.create_global::<JwmWaylandState>(&server.display.handle());
        server.state.outputs = vec![output];
        server.state.pointer_location = (8.0, 8.0).into();
        let mut app = App::new(&mut server);
        app.focus(&mut server);
        let mut ime = App::new(&mut server);
        let vk = virtual_keyboard(&mut server, &mut ime.client, ime.seat);
        let mut locker = App::new(&mut server);
        let manager = locker.client.bind("ext_session_lock_manager_v1", 1);
        let output = locker.client.bind("wl_output", 4);
        let lock = request_lock(&mut server, &mut locker.client, manager);
        let role = locker.client.new_id();
        locker.client.request(
            lock,
            GET_LOCK_SURFACE,
            &[role, locker.surface.id().protocol_id(), output],
        );
        server.roundtrip();
        assert!(server.state.session_locked);
        assert!(locked_test_key(&mut server.state, true));
        assert!(locked_test_key(&mut server.state, false));
        server.roundtrip();
        assert!(
            !locker.keys().is_empty(),
            "physical input must still reach the locker"
        );
        app.client.events();
        key(&mut ime.client, vk, 30, true);
        key(&mut ime.client, vk, 30, false);
        ime.client.request(vk, 2, &[1, 0, 0, 0]);
        server.roundtrip();
        assert!(app.client.events().is_empty());
        assert!(locker.client.events().is_empty());
    }

    #[test]
    fn virtual_keyboard_disconnect_releases_accepted_keys() {
        let mut server = Server::new();
        let mut app = App::new(&mut server);
        app.focus(&mut server);
        let mut ime = App::new(&mut server);
        let vk = virtual_keyboard(&mut server, &mut ime.client, ime.seat);
        key(&mut ime.client, vk, 30, true);
        server.roundtrip();
        assert_eq!(app.keys(), [(30, 1)]);
        drop(ime);
        server.roundtrip();
        server.roundtrip();
        assert_eq!(app.keys(), [(30, 0)]);
        assert!(server.state.virtual_keyboards.is_empty());
    }
}
