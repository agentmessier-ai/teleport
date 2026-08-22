//! The MCP tool surface and pi's registered tools must expose the same
//! parameters.
//!
//! This is the drift `docs/runtime-integration.md` opens by describing, and it
//! has happened repeatedly in the same direction: `mcp.rs` gets the new
//! capability, `integrations/pi/teleport.ts` does not, so the runtime that paid
//! to be integrated is the one still describing an older tool. Every instance so
//! far was found by a person noticing, never by anything structural — including
//! the one that prompted this file, where `since`/`until` reached MCP and pi's
//! wrappers still required a session id.
//!
//! A test cannot make the two implementations identical, but it can make them
//! disagree LOUDLY, which is all that was missing.

use std::collections::BTreeSet;
use std::path::PathBuf;

fn repo(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Parameter names declared for `tool` in the MCP `inputSchema`.
fn mcp_params(src: &str, tool: &str) -> BTreeSet<String> {
    let start = src
        .find(&format!("\"name\": \"{tool}\""))
        .unwrap_or_else(|| panic!("{tool} not found in mcp.rs"));
    // Past the `"properties": {` line itself, or the container name is read as
    // a parameter.
    let schema = src[start..]
        .find("\"properties\"")
        .map(|i| start + i + "\"properties\"".len())
        .expect("inputSchema properties");
    // Ends at the close of this tool's json! block — the next tool's `"name":`
    // is a reliable terminator, and so is end-of-list.
    let end = src[schema..]
        .find("\"name\": \"teleport_")
        .map(|i| schema + i)
        .unwrap_or(src.len());
    src[schema..end]
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let key = l.strip_prefix('"')?;
            let (name, rest) = key.split_once('"')?;
            rest.trim_start()
                .starts_with(": {")
                .then(|| name.to_string())
        })
        .collect()
}

/// Parameter names declared for `tool` in pi's `Type.Object({...})`.
fn pi_params(src: &str, tool: &str) -> BTreeSet<String> {
    let start = src
        .find(&format!("name: \"{tool}\""))
        .unwrap_or_else(|| panic!("{tool} not found in teleport.ts"));
    let end = src[start..]
        .find("async execute")
        .map(|i| start + i)
        .expect("execute block");
    src[start..end]
        .lines()
        .filter_map(|l| {
            let (name, rest) = l.trim().split_once(": Type.")?;
            // `parameters: Type.Object({` is the container, not a parameter.
            (!rest.starts_with("Object") && name.chars().all(|c| c.is_alphanumeric() || c == '_'))
                .then(|| name.to_string())
        })
        .collect()
}

/// Deliberate, explained differences. Anything not listed here is drift.
fn allowed(tool: &str, param: &str) -> bool {
    matches!(
        (tool, param),
        // MCP is called by a model that pages by echoing back a returned cursor,
        // so it takes raw ms. pi's `until` accepts unix ms as one of its
        // spellings, so the same paging works without a second parameter.
        ("teleport_turns", "before_ts")
            // MCP is a generic stdio server: it is spawned by whichever runtime
            // wants it and cannot know which session that is, so the caller has
            // to be able to state its own return address. pi's extension runs
            // INSIDE pi and reads it from `ctx`, so the parameter would be dead
            // weight there. A codex session found this the hard way — its reply
            // went out with no return address at all, arriving as "this message
            // cannot be replied to".
            | ("teleport_ask", "from_session")
            | ("teleport_note", "from_session")
            | ("teleport_reply", "from_session")
    )
}

/// Which tools each surface exposes AT ALL, which the parameter check above
/// cannot see: it compares tools the two already share.
///
/// That blind spot was real and had a symptom. `teleport_live` existed on pi and
/// dsh and NOT on MCP, so a Claude Code model asking "which sessions can I talk
/// to" reached for `teleport_sessions` — the ARCHIVE, including long-ended and
/// companion sessions. A session reported exactly that failure this morning:
/// fifteen rows of observer sessions burying its real work, rescued by `tp live`
/// from the CLI, which a model does not have. Meanwhile dsh went without
/// `teleport_note` from the moment it was added.
///
/// Both were found by a person asking "did you update all of them", which is the
/// same way every divergence before them was found.
fn tool_names(src: &str, pat: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = src;
    while let Some(i) = rest.find(pat) {
        rest = &rest[i + pat.len()..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        // NOT `strip_prefix("teleport_")`: `pat` already consumed that, so
        // stripping it again failed for every name and left both sets empty —
        // the test then passed by comparing nothing to nothing. Caught only by
        // deliberately breaking a surface and watching it stay green.
        if !name.is_empty() {
            out.insert(name);
        }
    }
    out
}

/// Tools a surface may legitimately lack, with the reason.
fn surface_exempt(surface: &str, tool: &str) -> bool {
    matches!(
        (surface, tool),
        // Pairing and LAN discovery are administrative: they change who this
        // machine trusts, and that is a human decision made at a terminal, not
        // something an agent should reach for mid-task. Only MCP carries them,
        // and only because Claude Code's own operator drives it there.
        (
            "pi",
            "discover"
                | "pair_request"
                | "pair_list"
                | "pair_approve"
                | "pair_reject"
                | "pair_revoke"
        ) | (
            "dsh",
            "discover"
                | "pair_request"
                | "pair_list"
                | "pair_approve"
                | "pair_reject"
                | "pair_revoke"
        )
            // pi drains its inbox through the `/tp` skill, which shells out to
            // the CLI — the capability is present, just not as a tool. `ack`
            // is the same story: the skill calls `tp ack <id>` directly.
            | ("pi", "inbox" | "ack")
    )
}

#[test]
fn every_surface_exposes_the_same_tools() {
    let mcp = tool_names(&repo("src/mcp.rs"), "\"name\": \"teleport_");
    let pi = tool_names(
        &repo("../../integrations/pi/teleport.ts"),
        "name: \"teleport_",
    );
    let dsh = tool_names(&repo("../../integrations/dsh/index.ts"), "name: 'teleport_");

    let mut problems = Vec::new();
    for (surface, have) in [("pi", &pi), ("dsh", &dsh)] {
        for missing in mcp.difference(have) {
            if !surface_exempt(surface, missing) {
                problems.push(format!(
                    "  {surface} is missing teleport_{missing} — a capability MCP callers have and {surface} callers do not"
                ));
            }
        }
        for extra in have.difference(&mcp) {
            problems.push(format!(
                "  MCP is missing teleport_{extra}, which {surface} has — the surface a model reaches for most is the one behind"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "runtime tool surfaces have drifted:\n{}\n\nAdd the tool, or add a documented exception to `surface_exempt`.",
        problems.join("\n")
    );
}

#[test]
fn pi_tools_expose_the_same_parameters_as_the_mcp_tools() {
    let mcp = repo("src/mcp.rs");
    let pi = repo("../../integrations/pi/teleport.ts");

    let mut problems = Vec::new();
    // ask/reply were outside this loop, which is why the divergence above
    // reached a real session: the suite covered the retrieval tools and left the
    // messaging ones — the half where a mismatch loses a conversation.
    for tool in [
        "teleport_search",
        "teleport_sessions",
        "teleport_turns",
        "teleport_ask",
        "teleport_note",
        "teleport_reply",
    ] {
        let m = mcp_params(&mcp, tool);
        let p = pi_params(&pi, tool);
        assert!(!m.is_empty(), "parsed no MCP params for {tool}");
        assert!(!p.is_empty(), "parsed no pi params for {tool}");

        for missing in m.difference(&p) {
            if !allowed(tool, missing) {
                problems.push(format!(
                    "{tool}: pi is missing {missing:?} — a capability MCP callers have and pi callers do not"
                ));
            }
        }
        for extra in p.difference(&m) {
            if !allowed(tool, extra) {
                problems.push(format!(
                    "{tool}: pi has {extra:?} but MCP does not — same name, two surfaces"
                ));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "MCP and pi tool surfaces have drifted:\n  {}\n\nFix integrations/pi/teleport.ts (or crates/tp/src/mcp.rs), \
         or add a documented exception to `allowed`.",
        problems.join("\n  ")
    );
}

/// One skill document, and it stays valid Agent Skills.
///
/// There used to be two — `plugin/skills/teleport/` and
/// `integrations/pi/skills/teleport/` — carrying the same advice in different
/// words. Measured before they were merged: 12 identical lines out of ~120,
/// both nonetheless covering the same two critical points (never report "never
/// happened" off a degraded scan; never rubber-stamp a pairing the user has not
/// compared out of band). Same facts, two hand-maintained wordings, drifting
/// independently for months without anything noticing.
///
/// Merging them was only half the fix. This is the half that keeps them merged,
/// because adding a third copy for codex and a fourth for dsh is the obvious
/// next move and the wrong one: `~/.agents/skills` is scanned by pi, codex AND
/// dsh, so one file already reaches all three.
///
/// The frontmatter assertions are the normative constraints from
/// <https://agentskills.io/specification>, checked here rather than by the
/// official `skills-ref validate` so the gate holds with no network and no npm.
#[test]
fn there_is_exactly_one_skill_and_it_matches_the_agent_skills_spec() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut found = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if p.is_dir() {
                // `target` is build output and `.git` is history; neither ships.
                if name != "target" && name != ".git" && name != "node_modules" {
                    stack.push(p);
                }
            } else if name == "SKILL.md" {
                found.push(p);
            }
        }
    }
    assert_eq!(
        found.len(),
        1,
        "expected exactly one SKILL.md — a second copy is how the last two drifted. Found: {found:#?}"
    );

    let path = &found[0];
    let text = std::fs::read_to_string(path).unwrap();
    let fm = text
        .strip_prefix("---\n")
        .and_then(|r| r.split_once("\n---\n"))
        .map(|(fm, _)| fm)
        .expect("SKILL.md must open with YAML frontmatter");

    let field = |k: &str| {
        fm.lines()
            .find_map(|l| l.strip_prefix(&format!("{k}: ")))
            .map(str::trim)
    };

    // name: 1-64 chars, lowercase alphanumeric and hyphens, no leading,
    // trailing or consecutive hyphens, and equal to the parent directory.
    let name = field("name").expect("`name` is required");
    assert!((1..=64).contains(&name.len()), "name length: {name:?}");
    assert!(
        name.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !name.starts_with('-')
            && !name.ends_with('-')
            && !name.contains("--"),
        "name must be kebab-case: {name:?}"
    );
    let parent = path
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy();
    assert_eq!(name, parent, "spec: `name` must match the parent directory");

    // description: 1-1024 characters, non-empty. It is the ONLY thing loaded at
    // startup, so an over-long one is a real failure rather than untidiness.
    let desc = field("description").expect("`description` is required");
    assert!(
        (1..=1024).contains(&desc.len()),
        "description is {} chars, spec allows 1-1024",
        desc.len()
    );

    // The body loads whole on activation; the spec recommends under 500 lines.
    let body_lines = text.lines().count();
    assert!(
        body_lines <= 500,
        "SKILL.md is {body_lines} lines, spec recommends <=500"
    );
}

/// `allowed-tools` must cover every tool the skill's body tells the model to
/// call. Split, they fail in the worst direction: the body says "call
/// `teleport_live`", the frontmatter does not grant it, and the model asks for
/// permission mid-task instead of doing the thing — a paper cut that looks like
/// the tool is broken.
///
/// Found by an outside review, not by this suite, and it was one commit old:
/// consolidating the two drifted SKILL.md copies added a sentence naming the
/// eight universal tools — including `_live` and `_note`, which the frontmatter
/// had never listed.
///
/// The two prefix families are both real and both kept. `mcp__teleport__*` is
/// what a directly-registered MCP server produces and
/// `mcp__plugin_teleport_teleport__*` is what the plugin produces, so a skill
/// that named only one would be half-granted depending on how teleport was
/// installed.
#[test]
fn every_tool_the_skill_names_is_also_granted() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugin/skills/teleport/SKILL.md");
    let text = std::fs::read_to_string(&path).unwrap();
    let (fm, body) = text
        .strip_prefix("---\n")
        .and_then(|r| r.split_once("\n---\n"))
        .expect("SKILL.md must open with frontmatter");

    let granted: std::collections::HashSet<&str> = fm
        .lines()
        .find(|l| l.starts_with("allowed-tools:"))
        .expect("`allowed-tools` is missing entirely")
        .split_whitespace()
        .filter(|t| t.starts_with("mcp__"))
        .filter_map(|t| t.rsplit("__").next())
        .collect();

    // The body writes the shared prefix once and then abbreviates —
    // "`teleport_search`, `_sessions`, `_turns`" — so both spellings count.
    let mut named: std::collections::HashSet<String> = std::collections::HashSet::new();
    for tok in body.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if let Some(rest) = tok.strip_prefix("teleport_") {
            if !rest.is_empty() {
                named.insert(format!("teleport_{rest}"));
            }
        }
    }
    for line in body.lines() {
        for tok in line.split('`') {
            if let Some(rest) = tok.strip_prefix('_') {
                if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                    named.insert(format!("teleport_{rest}"));
                }
            }
        }
    }

    let ungranted: Vec<&String> = named
        .iter()
        .filter(|t| !granted.contains(t.as_str()))
        .collect();
    assert!(
        ungranted.is_empty(),
        "the skill body tells the model to call these, and allowed-tools does not grant them: {ungranted:?}"
    );
}
