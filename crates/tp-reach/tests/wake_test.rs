//! `wake()`'s caller-capability gate (docs/same-machine-poke-design.md Gap 2):
//! `Target::Terminal` requires the calling process to hold/inherit the
//! Automation TCC grant, which `tpd` (a bare LaunchAgent) never has. This
//! must be refused explicitly rather than discovered as a runtime `-1743`.

#[test]
fn daemon_caller_refuses_iterm_backend() {
    let target = tp_reach::Target::Terminal {
        id: "iterm2".into(),
        tty: "/dev/ttys999".into(),
    };
    let err = tp_reach::wake(
        &target,
        "m/rt/s",
        tp_reach::CONTROL_STRING,
        tp_reach::Caller::Daemon,
    )
    .unwrap_err();
    assert!(err.to_string().contains("TCC-blocked"), "got: {err}");
}

#[test]
fn cli_caller_is_allowed_to_attempt_iterm_backend() {
    // No real iTerm2 session at this tty, so this exercises "allowed to try,
    // fails because the target doesn't exist" — NOT "refused outright",
    // which is the property this test is checking (that Cli isn't gated the
    // way Daemon is).
    let target = tp_reach::Target::Terminal {
        id: "iterm2".into(),
        tty: "/dev/ttys999-does-not-exist".into(),
    };
    let err = tp_reach::wake(
        &target,
        "m/rt/s",
        tp_reach::CONTROL_STRING,
        tp_reach::Caller::Cli,
    )
    .unwrap_err();
    assert!(
        !err.to_string().contains("TCC-blocked"),
        "Cli caller must not hit the daemon-only refusal, got: {err}"
    );
}

#[test]
fn unreachable_and_not_live_targets_are_always_a_silent_no_op() {
    // Mailbox-only degradation is intentional (LLD §7.2), for either caller.
    tp_reach::wake(
        &tp_reach::Target::Unreachable,
        "m/rt/s",
        tp_reach::CONTROL_STRING,
        tp_reach::Caller::Cli,
    )
    .unwrap();
    tp_reach::wake(
        &tp_reach::Target::NotLive,
        "m/rt/s",
        tp_reach::CONTROL_STRING,
        tp_reach::Caller::Daemon,
    )
    .unwrap();
}
