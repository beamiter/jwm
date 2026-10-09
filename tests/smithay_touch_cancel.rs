#![cfg(feature = "wayland-backends")]

#[cfg(test)]
#[allow(unused_variables)]
mod cancellation_regression_tests {
    use smithay::backend::input::InputTime;
    use smithay::backend::input::KeyState;
    use smithay::input::SeatState;
    use smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
    use smithay::input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
        GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
        GestureSwipeEndEvent, GestureSwipeUpdateEvent, MotionEvent as PointerMotionEvent,
        PointerTarget, RelativeMotionEvent,
    };
    use smithay::input::touch::{
        DownEvent, FrameMarker, MotionEvent, OrientationEvent, ShapeEvent, TouchHandle,
        TouchTarget, UpEvent,
    };
    use smithay::input::{Seat, SeatHandler};
    use smithay::utils::{IsAlive, Serial};
    use std::collections::HashMap;

    struct State {
        seat_state: SeatState<Self>,
        events: Vec<(u8, &'static str)>,
        last: HashMap<u8, FrameMarker>,
    }
    impl SeatHandler for State {
        type KeyboardFocus = Target;
        type PointerFocus = Target;
        type TouchFocus = Target;
        fn seat_state(&mut self) -> &mut SeatState<Self> {
            &mut self.seat_state
        }
    }
    #[derive(Debug, Clone, PartialEq)]
    struct Target(u8);
    impl IsAlive for Target {
        fn alive(&self) -> bool {
            true
        }
    }
    impl PointerTarget<State> for Target {
        fn enter(&self, seat: &Seat<State>, data: &mut State, event: &PointerMotionEvent) {}
        fn motion(&self, seat: &Seat<State>, data: &mut State, event: &PointerMotionEvent) {}
        fn relative_motion(
            &self,
            seat: &Seat<State>,
            data: &mut State,
            event: &RelativeMotionEvent,
        ) {
        }
        fn button(&self, seat: &Seat<State>, data: &mut State, event: &ButtonEvent) {}
        fn axis(&self, seat: &Seat<State>, data: &mut State, frame: AxisFrame) {}
        fn frame(&self, seat: &Seat<State>, data: &mut State) {}
        fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial, time: InputTime) {}
        fn gesture_swipe_begin(
            &self,
            seat: &Seat<State>,
            data: &mut State,
            event: &GestureSwipeBeginEvent,
        ) {
        }
        fn gesture_swipe_update(
            &self,
            seat: &Seat<State>,
            data: &mut State,
            event: &GestureSwipeUpdateEvent,
        ) {
        }
        fn gesture_swipe_end(
            &self,
            seat: &Seat<State>,
            data: &mut State,
            event: &GestureSwipeEndEvent,
        ) {
        }
        fn gesture_pinch_begin(
            &self,
            seat: &Seat<State>,
            data: &mut State,
            event: &GesturePinchBeginEvent,
        ) {
        }
        fn gesture_pinch_update(
            &self,
            seat: &Seat<State>,
            data: &mut State,
            event: &GesturePinchUpdateEvent,
        ) {
        }
        fn gesture_pinch_end(
            &self,
            seat: &Seat<State>,
            data: &mut State,
            event: &GesturePinchEndEvent,
        ) {
        }
        fn gesture_hold_begin(
            &self,
            seat: &Seat<State>,
            data: &mut State,
            event: &GestureHoldBeginEvent,
        ) {
        }
        fn gesture_hold_end(
            &self,
            seat: &Seat<State>,
            data: &mut State,
            event: &GestureHoldEndEvent,
        ) {
        }
    }
    impl KeyboardTarget<State> for Target {
        fn enter(
            &self,
            seat: &Seat<State>,
            data: &mut State,
            keys: Vec<KeysymHandle<'_>>,
            serial: Serial,
        ) {
        }
        fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial) {}
        fn key(
            &self,
            seat: &Seat<State>,
            data: &mut State,
            key: KeysymHandle<'_>,
            state: KeyState,
            serial: Serial,
            time: InputTime,
        ) {
        }
        fn modifiers(
            &self,
            seat: &Seat<State>,
            data: &mut State,
            modifiers: ModifiersState,
            serial: Serial,
        ) {
        }
    }
    impl TouchTarget<State> for Target {
        fn down(&self, seat: &Seat<State>, data: &mut State, event: &DownEvent) {
            data.events.push((self.0, "down"));
        }
        fn up(&self, seat: &Seat<State>, data: &mut State, event: &UpEvent) {
            data.events.push((self.0, "up"));
        }
        fn motion(&self, seat: &Seat<State>, data: &mut State, event: &MotionEvent) {
            data.events.push((self.0, "motion"));
        }
        fn frame(&self, seat: &Seat<State>, data: &mut State, marker: FrameMarker) {
            data.last.insert(self.0, marker);
            data.events.push((self.0, "frame"));
        }
        fn cancel(&self, seat: &Seat<State>, data: &mut State, marker: FrameMarker) {
            data.last.insert(self.0, marker);
            data.events.push((self.0, "cancel"));
        }
        fn shape(&self, seat: &Seat<State>, data: &mut State, event: &ShapeEvent) {}
        fn orientation(&self, seat: &Seat<State>, data: &mut State, event: &OrientationEvent) {}
        fn last_frame(&self, seat: &Seat<State>, data: &mut State) -> Option<FrameMarker> {
            data.last.get(&self.0).copied()
        }
    }
    fn setup() -> (State, TouchHandle<State>) {
        let mut state = State {
            seat_state: SeatState::new(),
            events: Vec::new(),
            last: HashMap::new(),
        };
        let mut seat = state.seat_state.new_seat("test");
        let touch = seat.add_touch();
        (state, touch)
    }
    fn down(state: &mut State, touch: &TouchHandle<State>, client: u8, slot: u32) {
        touch.down(
            state,
            Some((Target(client), (0.0, 0.0).into())),
            &DownEvent {
                slot: Some(slot).into(),
                location: (1.0, 1.0).into(),
                serial: (slot + 1).into(),
                time: InputTime::from_millis(0),
            },
        );
    }
    fn assert_retired(touch: &TouchHandle<State>, state: &mut State) {
        assert!(!touch.is_grabbed());
        // A cancelled up/down batch must not retain a pending target that
        // receives a late frame. Private slot/marker assertions live upstream.
        let events = state.events.clone();
        touch.frame(state);
        assert_eq!(state.events, events);
    }
    #[test]
    fn framed_contact_cancel_drops_current_grab_and_allows_fresh_sequence() {
        let (mut state, touch) = setup();
        down(&mut state, &touch, 1, 0);
        touch.frame(&mut state);
        assert!(touch.is_grabbed());
        touch.cancel(&mut state);
        assert_retired(&touch, &mut state);
        assert_eq!(state.events, [(1, "down"), (1, "frame"), (1, "cancel")]);
        down(&mut state, &touch, 2, 0);
        touch.frame(&mut state);
        assert!(touch.is_grabbed());
        assert_eq!(&state.events[3..], [(2, "down"), (2, "frame")]);
        touch.cancel(&mut state);
        assert_retired(&touch, &mut state);
    }
    #[test]
    fn cancellation_includes_unchanged_contacts_of_other_clients() {
        let (mut state, touch) = setup();
        down(&mut state, &touch, 1, 0);
        touch.frame(&mut state);
        touch.unset_grab(&mut state);
        down(&mut state, &touch, 2, 1);
        touch.cancel(&mut state);
        assert_retired(&touch, &mut state);
        let mut cancelled = state
            .events
            .iter()
            .filter_map(|(client, event)| (*event == "cancel").then_some(*client))
            .collect::<Vec<_>>();
        cancelled.sort();
        assert_eq!(cancelled, [1, 2]);
        assert!(state.events.iter().all(|(client, _)| *client != 3));
    }
    #[test]
    fn up_pending_frame_owner_is_cancelled_and_slot_is_retired() {
        let (mut state, touch) = setup();
        down(&mut state, &touch, 1, 0);
        touch.up(
            &mut state,
            &UpEvent {
                slot: Some(0).into(),
                serial: 9.into(),
                time: InputTime::from_millis(1),
            },
        );
        touch.cancel(&mut state);
        assert_retired(&touch, &mut state);
        assert_eq!(state.events, [(1, "down"), (1, "up"), (1, "cancel")]);
        touch.frame(&mut state);
        assert_eq!(state.events.len(), 3);
    }
    #[test]
    fn multiple_slots_of_one_client_receive_exactly_one_cancel() {
        let (mut state, touch) = setup();
        down(&mut state, &touch, 1, 0);
        down(&mut state, &touch, 1, 1);
        touch.cancel(&mut state);
        assert_retired(&touch, &mut state);
        assert_eq!(
            state
                .events
                .iter()
                .filter(|(_, event)| *event == "cancel")
                .count(),
            1
        );
    }
    #[test]
    fn empty_and_repeated_cancel_do_not_notify_untouched_clients() {
        let (mut state, touch) = setup();
        touch.cancel(&mut state);
        assert_retired(&touch, &mut state);
        assert!(state.events.is_empty());
        down(&mut state, &touch, 1, 0);
        touch.cancel(&mut state);
        let events = state.events.clone();
        touch.cancel(&mut state);
        assert_retired(&touch, &mut state);
        assert_eq!(state.events, events);
    }
    #[test]
    fn popup_pointer_authority_expires_on_release_and_uses_the_new_press() {
        use smithay::backend::input::ButtonState;
        let mut state = State {
            seat_state: SeatState::new(),
            events: Vec::new(),
            last: HashMap::new(),
        };
        let mut seat = state.seat_state.new_seat("popup-pointer-test");
        let pointer = seat.add_pointer();
        pointer.motion(
            &mut state,
            Some((Target(1), (0.0, 0.0).into())),
            &PointerMotionEvent {
                location: (1.0, 1.0).into(),
                serial: 1.into(),
                time: InputTime::from_millis(0),
            },
        );
        pointer.button(
            &mut state,
            &ButtonEvent {
                button: 0x110,
                state: ButtonState::Pressed,
                serial: 7.into(),
                time: InputTime::from_millis(1),
            },
        );
        assert!(pointer.has_grab(7.into()));
        assert_eq!(
            pointer
                .grab_start_data()
                .and_then(|data| data.focus)
                .map(|(target, _)| target),
            Some(Target(1))
        );
        pointer.button(
            &mut state,
            &ButtonEvent {
                button: 0x110,
                state: ButtonState::Released,
                serial: 8.into(),
                time: InputTime::from_millis(2),
            },
        );
        assert!(
            !pointer.has_grab(7.into()),
            "released serial cannot start a new popup chain"
        );
        pointer.button(
            &mut state,
            &ButtonEvent {
                button: 0x110,
                state: ButtonState::Pressed,
                serial: 9.into(),
                time: InputTime::from_millis(3),
            },
        );
        assert!(
            pointer.has_grab(9.into()),
            "a new actual press remains usable for a child popup"
        );
        assert!(!pointer.has_grab(7.into()));
        assert_eq!(
            pointer
                .grab_start_data()
                .and_then(|data| data.focus)
                .map(|(target, _)| target),
            Some(Target(1))
        );
    }
}
