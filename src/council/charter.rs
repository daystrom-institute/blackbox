//! Default council charter — the system-prompt-style block injected
//! into every dispatch as part of the council ambient layer.
//!
//! The charter establishes a multi-peer groupchat protocol:
//! - the bro is one participant among N
//! - turns are addressed `[name]`; @mentions force a response
//! - silence is a valid choice when there is nothing to add
//! - low-signal "no comment" replies are filtered before posting

pub const DEFAULT_CHARTER: &str = "\
You are ONE participant in a multi-peer groupchat. Other agents — and the user — \
are also participating. Chat turns are formatted as:

    [agent-name] body
    [user] body

Rules:
- If you are directly addressed (`@yourname`) you MUST respond.
- Otherwise responding is optional. You are encouraged to riff when it sharpens, \
expands, or corrects another participant — not for its own sake.
- A reply that adds no information (\"agreed\", \"no comment\", \"sounds good\") \
will be dropped before it reaches the transcript. If you have nothing to add, \
emit a single line `pass` and stop.
- Address other participants by name with `@name` when your reply is for them \
specifically; the runtime forwards the turn to the addressee.
- Cite file paths, line numbers, and concrete claims rather than gestures. \
Brevity is welcome; pad is not.";

/// Build the per-turn council ambient block. Goes into the prompt
/// AFTER `apply_ambient` (standard scope/recall/task-shape) but
/// BEFORE the turn body. Keeps council-specific framing together.
pub fn build_council_block(
    bro_name: &str,
    charter: &str,
    queue_depth: u32,
    addressed_by_user: bool,
    mentioned_by_bro: bool,
    replay_frame: Option<&str>,
    catchup_frame: Option<&str>,
) -> String {
    let mut out = String::new();
    out.push_str("[council]\n");
    out.push_str(&format!("your name: {bro_name}\n"));

    if queue_depth > 0 {
        out.push_str(&format!(
            "queue depth: {queue_depth} turn(s) waiting behind this one — be terse, \
             skip optional riffs\n"
        ));
    }
    if addressed_by_user {
        out.push_str("addressed: yes (the user mentioned you directly — you must respond)\n");
    } else if mentioned_by_bro {
        out.push_str("addressed: another bro mentioned you (response optional but encouraged)\n");
    }
    out.push('\n');

    out.push_str(charter);
    out.push_str("\n\n");

    if let Some(frame) = replay_frame {
        out.push_str("[council: replay]\n");
        out.push_str(
            "You joined this council mid-deliberation. The full prior transcript follows; \
             read it before contributing.\n\n",
        );
        out.push_str(frame.trim_end());
        out.push_str("\n\n");
    }

    if let Some(frame) = catchup_frame {
        out.push_str("[council: catchup]\n");
        out.push_str(
            "You fell behind. Several turns happened while your prior reply was in flight. \
             Review the missed turns; if anything still warrants a contribution — correction, \
             sharpening, missed angle — say it. If those points are already settled or the \
             conversation has moved past them, pass. One consolidated reply preferred over a \
             thread of micro-replies.\n\n",
        );
        out.push_str(frame.trim_end());
        out.push_str("\n\n");
    }

    out
}
