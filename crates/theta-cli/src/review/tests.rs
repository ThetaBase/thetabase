//! The review screen's rules, tested as rules rather than as rendering.

use theta_proto::wire::{ChangeDiffWire, GateWire, ReviewBatchWire};

use super::app::{Action, Authority, Key, Mode, ReviewApp, DORMANT_AFTER_MS};
use super::render;

fn change(id: &str, gate: GateWire, rows: u64) -> ChangeDiffWire {
    ChangeDiffWire {
        change_id: id.into(),
        destructive: gate != GateWire::AutoApply,
        rows_affected: rows,
        reversible: gate == GateWire::AutoApply,
        gate,
        reason: "A destructive change on a protected branch needs a human.".into(),
        affected_table: "customers".into(),
        affected_column: "legacy_ref".into(),
        change_type: "drop_column".into(),
        shadow_branch_id: match gate {
            GateWire::ShadowValidate => 2,
            _ => 0,
        },
        ..Default::default()
    }
}

fn batch(key: &str, changes: Vec<ChangeDiffWire>) -> ReviewBatchWire {
    ReviewBatchWire {
        key: key.into(),
        gate: 2,
        rows_affected: changes.iter().map(|c| c.rows_affected).sum(),
        changes,
        ..Default::default()
    }
}

fn app(changes: Vec<ChangeDiffWire>) -> ReviewApp {
    ReviewApp::new(vec![batch("customers", changes)], Authority::MayReview)
}

// ---- there is no bulk confirm -----------------------------------------------

#[test]
fn one_keystroke_can_never_answer_more_than_one_gate() {
    // The rule the whole screen exists to preserve. A select-all that answered
    // every pending gate is the feature most likely to be asked for and the one
    // that would void the product's central claim — the gate exists so a person
    // decides, and a batch confirm makes deciding a formality.
    //
    // `on_key` returning `Option<Action>` is the structural half: a keystroke
    // that answers two gates is unrepresentable, so this cannot be broken by
    // somebody adding a convenience without changing the signature.
    let mut app = app(vec![
        change("chg_1", GateWire::Confirm, 5),
        change("chg_2", GateWire::Confirm, 9),
        change("chg_3", GateWire::Confirm, 2),
    ]);

    // Every key on the keyboard, against a full queue.
    for key in [
        Key::Char('c'),
        Key::Char('a'),
        Key::Char('A'),
        Key::Char('*'),
        Key::Enter,
        Key::Char('y'),
    ] {
        let action = app.on_key(key);
        if let Some(Action::Confirm(id)) = &action {
            assert!(
                ["chg_1", "chg_2", "chg_3"].contains(&id.as_str()),
                "a keystroke confirmed something that is not one of the pending \
                 changes: {id}"
            );
        }
        // The type makes "confirmed two" unrepresentable; this asserts the
        // intent alongside it so the reason survives a refactor.
        assert!(
            matches!(
                action,
                None | Some(Action::Confirm(_))
                    | Some(Action::Promote(_))
                    | Some(Action::Quit)
                    | Some(Action::Refresh)
                    | Some(Action::Reject { .. })
            ),
            "a keystroke produced something other than a single action"
        );
    }
}

// ---- the strongest gate costs more than a keypress --------------------------

#[test]
fn confirming_a_shadow_validated_change_requires_typing_its_id() {
    // Four seconds against dropping a column from a protected branch. Not a
    // modal asking "are you sure" — those train people to dismiss modals — but
    // an act proportionate to what is about to happen.
    let mut app = app(vec![change("chg_8f2a", GateWire::ShadowValidate, 48_201)]);

    assert_eq!(app.on_key(Key::Char('c')), None, "a keypress confirmed it");
    assert!(matches!(app.mode(), Mode::ConfirmingById { .. }));

    // A wrong id confirms nothing and says so.
    for c in "chg_wrong".chars() {
        assert_eq!(app.on_key(Key::Char(c)), None);
    }
    assert_eq!(app.on_key(Key::Enter), None);
    assert!(app
        .message()
        .unwrap_or_default()
        .contains("not the change id"));
}

#[test]
fn typing_the_right_id_does_confirm_it() {
    // The gate is a question, not a wall. `TERMS.md` §2.1 says a change you
    // confirm is a change you asked for, and a screen that made confirmation
    // impossible would make that sentence false in the other direction.
    let mut app = app(vec![change("chg_8f2a", GateWire::ShadowValidate, 48_201)]);
    app.on_key(Key::Char('c'));

    for c in "chg_8f2a".chars() {
        assert_eq!(app.on_key(Key::Char(c)), None);
    }
    assert_eq!(
        app.on_key(Key::Enter),
        Some(Action::Confirm("chg_8f2a".into()))
    );
}

#[test]
fn a_merely_confirmable_change_does_not_need_typing() {
    // The friction is proportionate or it is theatre. Making every gate cost a
    // typed id would train people to type ids, which is the same reflex the
    // shadow-validate case is trying to avoid.
    let mut app = app(vec![change("chg_1", GateWire::Confirm, 3)]);
    assert_eq!(
        app.on_key(Key::Char('c')),
        Some(Action::Confirm("chg_1".into()))
    );
}

#[test]
fn escaping_out_of_a_confirmation_confirms_nothing() {
    let mut app = app(vec![change("chg_8f2a", GateWire::ShadowValidate, 1)]);
    app.on_key(Key::Char('c'));
    for c in "chg_8f2a".chars() {
        app.on_key(Key::Char(c));
    }
    assert_eq!(app.on_key(Key::Esc), None);
    assert_eq!(*app.mode(), Mode::Browsing);
}

// ---- the queue is triage's order --------------------------------------------

#[test]
fn the_queue_is_shown_in_the_order_the_server_sent_it() {
    // `safety::triage` already produces a total order. A second opinion about
    // urgency rendered on top of the first is how a reviewer ends up reading
    // them in an order nobody intended — and the most urgent change is the one
    // that would move.
    let app = ReviewApp::new(
        vec![
            batch(
                "orders",
                vec![change("chg_z", GateWire::ShadowValidate, 900)],
            ),
            batch("customers", vec![change("chg_a", GateWire::Confirm, 1)]),
        ],
        Authority::MayReview,
    );

    let ids: Vec<&str> = app.changes().iter().map(|c| c.change_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["chg_z", "chg_a"],
        "the queue was re-sorted; triage's order is the one that matters"
    );
}

#[test]
fn a_refresh_keeps_the_reviewer_on_the_change_they_were_reading() {
    // Following the id rather than the index. A change landing above the cursor
    // would otherwise move the selection under somebody's hands, and the
    // keystroke they were about to press would answer a different change.
    let mut app = app(vec![
        change("chg_1", GateWire::Confirm, 1),
        change("chg_2", GateWire::Confirm, 2),
    ]);
    app.on_key(Key::Down);
    assert_eq!(app.selected().unwrap().change_id, "chg_2");

    app.update(vec![batch(
        "customers",
        vec![
            change("chg_new", GateWire::Confirm, 7),
            change("chg_1", GateWire::Confirm, 1),
            change("chg_2", GateWire::Confirm, 2),
        ],
    )]);

    assert_eq!(
        app.selected().unwrap().change_id,
        "chg_2",
        "a new change arriving moved the selection under the reviewer"
    );
}

#[test]
fn a_refresh_that_removes_the_selected_change_does_not_leave_a_dangling_cursor() {
    let mut app = app(vec![
        change("chg_1", GateWire::Confirm, 1),
        change("chg_2", GateWire::Confirm, 2),
    ]);
    app.on_key(Key::Down);

    app.update(vec![batch(
        "customers",
        vec![change("chg_1", GateWire::Confirm, 1)],
    )]);

    assert_eq!(app.selected().unwrap().change_id, "chg_1");
}

// ---- authority ---------------------------------------------------------------

#[test]
fn an_observer_can_read_the_queue_and_cannot_answer_it() {
    // Understanding what is waiting is not the same privilege as answering it,
    // and hiding the queue would make the system less legible for no security
    // gain.
    let mut app = ReviewApp::new(
        vec![batch(
            "customers",
            vec![change("chg_1", GateWire::Confirm, 5)],
        )],
        Authority::Observer {
            needs_role: "reviewer".into(),
        },
    );

    assert_eq!(app.changes().len(), 1, "an observer cannot see the queue");

    for key in [Key::Char('c'), Key::Char('p'), Key::Char('r')] {
        assert_eq!(app.on_key(key), None, "an observer answered a gate");
    }
    let message = app.message().unwrap_or_default();
    assert!(
        message.contains("reviewer"),
        "the refusal has to name the role that would help, or somebody has to \
         go and research it: {message}"
    );
}

#[test]
fn the_footer_does_not_offer_actions_an_observer_cannot_take() {
    // Offering `c` to somebody without the role invites them to press it and be
    // refused, which teaches them the screen is unreliable rather than that
    // they lack a permission.
    let app = ReviewApp::new(
        vec![batch(
            "customers",
            vec![change("chg_1", GateWire::Confirm, 1)],
        )],
        Authority::Observer {
            needs_role: "reviewer".into(),
        },
    );

    let footer = render::footer(&app);
    assert!(!footer.contains("[c]"), "{footer}");
    assert!(footer.contains("reviewer"), "{footer}");
}

// ---- what the reviewer is shown ---------------------------------------------

#[test]
fn the_detail_names_the_blast_radius_the_gate_and_whether_it_can_be_undone() {
    // A confirmation screen that hid the row count would still get confirmed,
    // and the person doing it would have been entitled to assume it was small.
    let app = app(vec![change("chg_8f2a", GateWire::ShadowValidate, 48_201)]);
    let detail = render::detail(&app).join("\n");

    assert!(
        detail.contains("48201"),
        "the row count is missing: {detail}"
    );
    assert!(detail.contains("customers.legacy_ref"), "{detail}");
    assert!(detail.contains("Reversible"), "{detail}");
    assert!(detail.contains("shadow"), "{detail}");
    assert!(
        detail.contains("needs a human"),
        "the reason the gate fired is the sentence the whole screen turns on: \
         {detail}"
    );
}

#[test]
fn the_title_says_how_many_are_destructive_rather_than_only_how_many_wait() {
    // "Three waiting" and "three waiting, one destructive" are different
    // situations and only one of them needs attention now.
    let app = app(vec![
        change("chg_1", GateWire::AutoApply, 1),
        change("chg_2", GateWire::ShadowValidate, 900),
    ]);
    let title = render::title(&app, "acme/checkout");
    assert!(title.contains("2 waiting"), "{title}");
    assert!(title.contains("1 destructive"), "{title}");
}

#[test]
fn an_empty_queue_says_so_rather_than_rendering_nothing() {
    let app = ReviewApp::new(Vec::new(), Authority::MayReview);
    assert!(render::queue(&app).join("").contains("Nothing"));
    assert!(render::detail(&app).join("").contains("Nothing"));
}

// ---- rejection ----------------------------------------------------------------

#[test]
fn a_rejection_without_a_reason_is_refused() {
    // The agent reads this. A refusal with no reason is one it cannot act on,
    // so it proposes the same thing again and the reviewer sees it twice.
    let mut app = app(vec![change("chg_1", GateWire::Confirm, 1)]);
    app.on_key(Key::Char('r'));

    assert_eq!(app.on_key(Key::Enter), None);
    assert!(app.message().unwrap_or_default().contains("needs a reason"));

    for c in "column is still in use".chars() {
        app.on_key(Key::Char(c));
    }
    assert_eq!(
        app.on_key(Key::Enter),
        Some(Action::Reject {
            change_id: "chg_1".into(),
            reason: "column is still in use".into()
        })
    );
}

// ---- the cost of leaving it open ---------------------------------------------

#[test]
fn a_screen_nobody_is_touching_stops_polling() {
    // Answering a poll requires the instance to be *running*, so there is no
    // server-side way to treat polling as idle — this is the client-side half.
    // A terminal left open in a tmux pane for a week would otherwise pay to
    // keep a development instance awake indefinitely, which is exactly what
    // hibernation exists to stop.
    let mut app = app(vec![change("chg_1", GateWire::Confirm, 1)]);
    assert!(app.should_poll());

    app.tick(DORMANT_AFTER_MS - 1);
    assert!(app.should_poll(), "it went dormant early");

    app.tick(1);
    assert!(!app.should_poll(), "an untouched screen kept polling");
}

#[test]
fn the_keypress_that_wakes_a_dormant_screen_does_not_also_answer_a_gate() {
    // Otherwise the first thing somebody types after a break confirms whatever
    // the cursor happens to be on.
    let mut app = app(vec![change("chg_1", GateWire::Confirm, 1)]);
    app.tick(DORMANT_AFTER_MS);

    assert_eq!(app.on_key(Key::Char('c')), Some(Action::Refresh));
    assert!(app.should_poll());
    assert_eq!(*app.mode(), Mode::Browsing);
}

#[test]
fn typing_keeps_the_screen_awake() {
    let mut app = app(vec![change("chg_1", GateWire::Confirm, 1)]);
    for _ in 0..10 {
        app.tick(DORMANT_AFTER_MS - 1);
        app.on_key(Key::Down);
    }
    assert!(app.should_poll(), "a screen in use went dormant");
}
