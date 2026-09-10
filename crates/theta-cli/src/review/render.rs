//! Turning the review state into lines of text.
//!
//! Separate from the terminal, and returning `String`s rather than drawing, so
//! what a reviewer is shown can be asserted. The detail pane is the whole
//! product — a gate that fired without saying what it was protecting would be a
//! spinner — so its contents are a tested property rather than a rendering
//! detail.

use theta_proto::wire::{ChangeDiffWire, GateWire};

use super::app::{Authority, Mode, ReviewApp};

/// The list on the left.
pub fn queue(app: &ReviewApp) -> Vec<String> {
    let changes = app.changes();
    if changes.is_empty() {
        return vec!["Nothing is waiting.".into()];
    }

    changes
        .iter()
        .enumerate()
        .map(|(i, change)| {
            let marker = match i == app.selected_index() {
                true => "▸",
                false => " ",
            };
            format!("{marker} {}  {}", change.change_id, describe(change))
        })
        .collect()
}

/// One line saying what a change does.
fn describe(change: &ChangeDiffWire) -> String {
    let target = match change.affected_column.is_empty() {
        true => change.affected_table.clone(),
        false => format!("{}.{}", change.affected_table, change.affected_column),
    };
    format!("{} {}", change.change_type.replace('_', " "), target)
}

/// The pane on the right.
///
/// Everything a reviewer needs in order for their confirmation to mean
/// something. The row count in particular: a confirmation screen that hid the
/// blast radius would still get confirmed, and the person doing it would have
/// been entitled to assume it was small.
pub fn detail(app: &ReviewApp) -> Vec<String> {
    let Some(change) = app.selected() else {
        return vec!["Nothing is waiting for you.".into()];
    };

    let mut lines = vec![
        change.change_id.clone(),
        "─".repeat(46),
        describe(change),
        String::new(),
        format!("{:<12} {}", "Gate", gate_name(change.gate)),
        format!("{:<12} {} row(s) affected", "Impact", change.rows_affected),
        format!(
            "{:<12} {}",
            "Reversible",
            match change.reversible {
                true => "yes",
                false => "no",
            }
        ),
        format!(
            "{:<12} {}",
            "Destructive",
            match change.destructive {
                true => "yes",
                false => "no",
            }
        ),
    ];

    if !change.reason.is_empty() {
        lines.push(String::new());
        lines.push(change.reason.clone());
    }

    if change.shadow_branch_id != 0 {
        lines.push(String::new());
        lines.push(format!(
            "Validated on shadow branch {}",
            change.shadow_branch_id
        ));
    }

    lines
}

fn gate_name(gate: GateWire) -> &'static str {
    match gate {
        GateWire::AutoApply => "applied",
        GateWire::Confirm => "needs a confirmation",
        GateWire::ShadowValidate => "shadow validated — needs promotion",
    }
}

/// The line along the bottom.
///
/// Shows only what the reviewer can actually do. Offering `c` to somebody
/// without the role is an invitation to press it and be refused, which teaches
/// them the screen is unreliable rather than that they lack a permission.
pub fn footer(app: &ReviewApp) -> String {
    match app.mode() {
        Mode::ConfirmingById { change_id, typed } => {
            format!("Confirm {change_id}: {typed}▏   [esc] cancel")
        }
        Mode::RejectingWithReason { typed, .. } => {
            format!("Reason: {typed}▏   [enter] send  [esc] cancel")
        }
        Mode::Dormant => "Paused. [any key] resume".into(),
        Mode::Browsing => match app.authority() {
            Authority::MayReview => {
                "[↑↓] move  [c] confirm  [p] promote  [r] reject  [q] quit".into()
            }
            Authority::Observer { needs_role } => {
                format!("Read-only — answering a gate needs `{needs_role}`.  [q] quit")
            }
        },
    }
}

/// The heading.
pub fn title(app: &ReviewApp, project: &str) -> String {
    let waiting = app.changes().len();
    let destructive = app.changes().iter().filter(|c| c.destructive).count();

    match destructive {
        0 => format!("Review — {project} · {waiting} waiting"),
        n => format!("Review — {project} · {waiting} waiting, {n} destructive"),
    }
}
