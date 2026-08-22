#!/bin/bash
# Install tp + tpd and register the LaunchAgent (LLD §12).
#
# Usage:  ./install/install.sh          install and start
#         ./install/install.sh --uninstall
set -euo pipefail

LABEL="io.teleport.tpd"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
BIN_DIR="$HOME/.local/bin"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Not `rust-version` in Cargo.toml, which is a different and weaker claim. That
# field is only consulted AFTER every manifest in the graph parses, and on a
# 1.79 toolchain the parse is what fails first — five levels below anything
# teleport wrote:
#
#   error: failed to parse manifest at .../cpubits-0.1.1/Cargo.toml
#   Caused by: feature `edition2024` is required
#     ...not stabilized in this version of Cargo (1.79.0).
#     Consider trying a newer version of Cargo (this may require the nightly
#     release).
#
# Measured on a real machine, not imagined. teleport is never named, the crate
# that is named is a transitive dependency of a transitive dependency
# (tp-net -> httpsig -> ecdsa -> elliptic-curve -> crypto-bigint -> cpubits),
# and the advice points at nightly when plain stable would do. Checking here is
# what turns that into a sentence about teleport.
MIN_RUST="1.88.0"

# `command -v` alone is not enough. A login shell, a script over ssh and a
# launchd job see three different PATHs, and telling someone with
# /usr/local/bin/cargo that Rust is not installed sends them to rustup.rs to fix
# a problem they do not have. Borrowed from hermes-agent's installer, which
# looks in three places for `uv` before concluding it is absent.
find_cargo() {
    local c
    for c in cargo "$HOME/.cargo/bin/cargo" /usr/local/bin/cargo /opt/homebrew/bin/cargo; do
        if command -v -- "$c" >/dev/null 2>&1; then
            command -v -- "$c"
            return 0
        fi
    done
    return 1
}

# `sort -V`, not three integer comparisons: hand-rolled component compare is
# where this check usually goes wrong (1.10 vs 1.9), and every machine that can
# run this script has sort.
version_lt() {
    [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -1)" = "$1" ]
}

preflight() {
    # A source tree builds; an unpacked release archive already has what it
    # needs. `release.yml` ships install.sh next to `tp` and `tpd` with no
    # crates, so demanding a toolchain there would be asking for a compiler to
    # build something already built.
    if [ ! -f "$REPO/Cargo.toml" ]; then
        if [ -x "$REPO/tp" ] && [ -x "$REPO/tpd" ]; then
            PREBUILT=1
            return 0
        fi
        echo "install.sh cannot tell what it is installing FROM." >&2
        echo "  Expected either $REPO/Cargo.toml (a source checkout)" >&2
        echo "  or tp and tpd next to this script (an unpacked release)." >&2
        echo "  Found neither. If you copied install.sh on its own, take the" >&2
        echo "  whole tree: https://github.com/agentmessier-ai/teleport" >&2
        exit 1
    fi
    PREBUILT=0

    local cargo_bin ver
    if ! cargo_bin="$(find_cargo)"; then
        echo "teleport builds from source and needs a Rust toolchain." >&2
        echo "  Install:  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
        echo "  Then:     $0" >&2
        echo >&2
        echo "  rustup specifically, not Homebrew's rust: this repository pins its" >&2
        echo "  toolchain in rust-toolchain.toml, which only rustup reads. With" >&2
        echo "  rustup the right version is fetched for you." >&2
        exit 1
    fi
    ver="$("$cargo_bin" --version 2>/dev/null | awk '{print $2}')"
    if [ -z "$ver" ]; then
        echo "warning: $cargo_bin did not report a version — continuing anyway." >&2
        return 0
    fi
    if version_lt "$ver" "$MIN_RUST"; then
        # Name the version AND the path. "Rust is too old" is unactionable on a
        # machine with three of them installed.
        echo "teleport needs Rust >= $MIN_RUST; this shell has $ver ($cargo_bin)." >&2
        if command -v rustup >/dev/null 2>&1; then
            echo "  rustup is managing this toolchain. Run:  rustup update" >&2
            echo "  rust-toolchain.toml then pins the build to the right version." >&2
        else
            echo >&2
            echo "  There is no rustup here, so rust-toolchain.toml — which pins the" >&2
            echo "  version this is tested at — does nothing on this machine." >&2
            echo "  Installing rustup fixes both at once:" >&2
            echo "    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
        fi
        exit 1
    fi
}
preflight

# `bootout` on a service that isn't loaded exits non-zero — that is a normal
# state here, not a failure, so it must not trip `set -e`.
unload() { launchctl bootout "gui/$(id -u)/$LABEL" 2>/dev/null || true; }

if [[ "${1:-}" == "--uninstall" ]]; then
    unload
    rm -f "$PLIST"
    # The plugin owns the hooks, /tp, the skill and the MCP entry, so removing
    # it removes all four — which is the point of having used the package
    # manager to install them. Guarded on `claude` existing, and tolerant of
    # failure: a machine that never had the plugin has nothing to uninstall,
    # and that is not an error worth stopping a teardown for.
    if command -v claude >/dev/null 2>&1; then
        claude plugin uninstall teleport@teleport >/dev/null 2>&1 || true
        claude plugin marketplace remove teleport >/dev/null 2>&1 || true
    fi
    # Pre-plugin leftovers. Kept because an install from before the plugin
    # carried these wrote them by hand, and nothing else will ever clean them
    # up; both are no-ops on a machine that never had them.
    if [ -f "$REPO/install/setup-hooks.py" ] && command -v python3 >/dev/null 2>&1; then
        python3 "$REPO/install/setup-hooks.py" --uninstall || true
    fi
    rm -f "$HOME/.claude/commands/tp.md"
    rm -f "$HOME/.pi/agent/extensions/teleport.ts"
    rm -rf "$HOME/.pi/agent/skills/teleport"   # pre-0.2.1 location
    rm -rf "$HOME/.agents/skills/teleport"
    # Only the configs THIS repo ships — a config the operator wrote for their
    # own runtime lives in the same directory and is not ours to delete.
    for f in "$REPO/install/runtimes.d/"*.toml; do
        rm -f "$HOME/.teleport/runtimes.d/$(basename "$f")"
    done
    echo "uninstalled. Binaries in $BIN_DIR and data in ~/.teleport were left alone."
    echo "  rm -rf ~/.teleport   # to also drop the index, key and pairings"
    exit 0
fi

# ── Gate: refuse to install a commit CI has not passed ────────────────────────
#
# This script IS teleport's deploy — the moment code becomes running software
# on a machine. Everywhere else in this fleet the gates hang off a deploy job's
# `needs:`; here the equivalent seam is this check, because there is no server
# to deploy to and branch protection is unavailable on this account, so
# nothing stops a red commit from being merged and
# then installed.
#
# Three states, deliberately different:
#   clean + pushed  → require a green CI run for THIS commit. This is what
#                     makes the CI result load-bearing instead of decorative.
#   dirty or local  → warn only. Refusing here would make the script useless
#                     for the person developing teleport, which is most of its
#                     use; they are installing something CI has never seen and
#                     the message says so.
#   no gh / offline → warn only. A missing tool must not become a silent skip
#                     that looks like a pass.
#
# TP_SKIP_CI_GATE=1 overrides. An override that has to be typed is a decision;
# one that happens by default is an accident.
#
# SCOPE — this protects the person who BUILDS from a checkout, which today is
# whoever develops teleport. It does very little for a downstream user of an
# open-source release: they usually have no `gh`, no auth, and no reason to
# trust an API answer more than the source they already hold. What protects
# THEM is signed release artifacts with checksums, and this repo publishes no
# releases yet (OpenSSF Scorecard scores Signed-Releases as N/A for exactly
# that reason). Do not mistake this check for that one.
gate_ci() {
    [ "${TP_SKIP_CI_GATE:-}" = "1" ] && { echo "  CI gate: SKIPPED (TP_SKIP_CI_GATE=1)"; return 0; }

    # Every check below asks about a git checkout: is it dirty, is the commit
    # pushed, did CI pass for that sha. An unpacked release archive has no
    # commit — its provenance is the release it was downloaded from, which is a
    # different question this gate cannot answer. Saying nothing beats reporting
    # a missing answer to a question that was not asked.
    if [ "$PREBUILT" = "1" ]; then
        return 0
    fi

    if ! command -v gh >/dev/null 2>&1; then
        # NOT "installing unverified". That phrasing implies a check that should
        # have happened and did not, and on the public repository no check was
        # ever going to run — see the sibling message below, which was fixed
        # first and left this branch, the one most outside users actually reach,
        # still saying it.
        echo "  CI gate: gh not installed — nothing to check against."
        return 0
    fi
    if [ -n "$(git -C "$REPO" status --porcelain 2>/dev/null)" ]; then
        echo "  CI gate: working tree is dirty — installing a build CI has never seen."
        return 0
    fi

    local sha
    sha="$(git -C "$REPO" rev-parse HEAD 2>/dev/null)" || return 0
    if ! git -C "$REPO" branch -r --contains "$sha" >/dev/null 2>&1 \
       || [ -z "$(git -C "$REPO" branch -r --contains "$sha" 2>/dev/null)" ]; then
        echo "  CI gate: $sha is not pushed — installing a build CI has never seen."
        return 0
    fi

    # Derived from origin, never hardcoded: a fork must be checked against its
    # OWN CI. Hardcoding upstream would tell a fork's users that upstream is
    # green while installing the fork's code, which is worse than not checking.
    local slug conclusion
    slug="$(git -C "$REPO" remote get-url origin 2>/dev/null \
        | sed -E 's#^git@github\.com:#https://github.com/#; s#\.git$##' \
        | sed -E 's#^https://github\.com/##')"
    # owner/repo, and nothing that survived the sed untouched. A bare `*/*`
    # test is not enough: a GitLab https URL and a Bitbucket ssh URL both keep
    # a slash, and both got through it when tested.
    case "$slug" in
        *://*|*@*|*:*|*" "*|*/*/*|*/) slug="" ;;  # kept a scheme, host or extra path
        */*) ;;                                    # owner/repo — the only shape we accept
        *) slug="" ;;
    esac
    if [ -z "$slug" ]; then
        echo "  CI gate: origin is not a GitHub repo — cannot check."
        return 0
    fi

    conclusion="$(gh run list --repo "$slug" --commit "$sha" \
        --workflow=ci.yml --limit 1 --json conclusion --jq '.[0].conclusion' 2>/dev/null)"

    case "$conclusion" in
        success)
            echo "  CI gate: green for ${sha:0:7}" ;;
        "")
            # "unverified" implied a check that should have happened and did
            # not. In the published repository nothing runs on a push — the only
            # workflow there builds release binaries from a tag — so every
            # outside install would read a normal state as a warning.
            echo "  CI gate: no CI run for ${sha:0:7} — nothing to check against." ;;
        *)
            echo "" >&2
            echo "  CI gate: REFUSING — ci.yml concluded '$conclusion' for ${sha:0:7}." >&2
            echo "  https://github.com/$slug/actions" >&2
            echo "  Override with TP_SKIP_CI_GATE=1 if you know why." >&2
            exit 1 ;;
    esac
}
gate_ci

# An unpacked release archive is already built; the CI gate above is about a
# source checkout's provenance and has nothing to say about a downloaded
# tarball, whose provenance is the release it came from.
if [ "$PREBUILT" = "1" ]; then
    echo "using the binaries shipped in this archive (no build needed)"
    SRC_BIN="$REPO"
else
SRC_BIN="$REPO/target/release"
echo "building release binaries…"
# cargo-auditable embeds the dependency tree into the binary, so an INSTALLED
# tp can be audited later with `cargo audit bin ~/.local/bin/tp` — without it
# there is no way to answer "what is inside the copy someone installed three
# months ago", which is the gap this build-on-your-own-machine model creates.
# Degrades to a plain build rather than forcing a toolchain install on every
# user; the note tells them what they are giving up.
if command -v cargo-auditable >/dev/null 2>&1; then
    cargo auditable build --release --manifest-path "$REPO/Cargo.toml" -p tp
else
    echo "  note: cargo-auditable not found — building without an embedded SBOM."
    echo "        \`cargo install cargo-auditable\` to make installed binaries auditable."
    cargo build --release --manifest-path "$REPO/Cargo.toml" -p tp
fi
fi

mkdir -p "$BIN_DIR" "$HOME/.teleport" "$HOME/Library/LaunchAgents"
# Install by atomic RENAME, not by writing over the destination.
#
# `install`/`cp` open the target with O_TRUNC, which rewrites the inode a
# RUNNING process has mapped. macOS then fails the code-signature check on the
# next page fault and kills it — `launchctl print io.teleport.tpd` showed
# `last exit reason = OS_REASON_CODESIGNING` from exactly this. A rename gives
# the new binary a fresh inode, so a running tpd keeps its own until it exits
# and launchd only ever starts a complete file.
#
# The signature itself needs no help: cargo's linker ad-hoc signs the binary
# (`codesign -dv` reports `adhoc,linker-signed`) and both cp and rename preserve
# it, so there is no `codesign` step to get wrong.
for b in tp tpd; do
    install -m 755 "$SRC_BIN/$b" "$BIN_DIR/.$b.new"
    mv -f "$BIN_DIR/.$b.new" "$BIN_DIR/$b"
done

# Runtime descriptors (LLD §5) are NOT copied: the binary carries them
# (`include_str!`, see decl.rs::EMBEDDED), and a file of the same id in
# ~/.teleport/runtimes.d is for USER overrides only. This step used to copy the
# shipped files out, which is how every install seeded a future staleness bug:
# the copies outlived the binaries that matched them, silently overrode newer
# embedded text, and twice in one session made a rebuilt binary run
# "byte-identical wrong" with parsing rules it no longer contained.
#
# Clean up what past installs left behind — but ONLY copies byte-identical to
# what this build embeds, where removal is provably a no-op. A file that
# differs is customized or stale, and content cannot say which: it stays, it
# wins (that is the override contract), and `tp version` names it.
mkdir -p "$HOME/.teleport/runtimes.d"
for f in "$REPO/install/runtimes.d/"*.toml; do
    dst="$HOME/.teleport/runtimes.d/$(basename "$f")"
    if [ -f "$dst" ] && cmp -s "$f" "$dst"; then
        rm "$dst"
        echo "  removed $dst — identical to the descriptor embedded in this build"
    elif [ -f "$dst" ]; then
        echo "  kept $dst — differs from the embedded descriptor (customized or stale; see \`tp version\`)"
    fi
done

# Write beside it, then rename — the same discipline the binaries above already
# use, for the same reason and one more.
#
# `cmd > "$PLIST"` truncates the destination BEFORE the command runs, so any
# failure in the pipeline leaves a zero-byte plist behind. That is not
# hypothetical: it happened here while testing an archive whose `install/`
# directory was incomplete. sed exited non-zero, `set -e` stopped the script,
# and what remained was a valid path containing nothing — `launchctl bootstrap`
# answers "Input/output error 5" for that, which names neither the file nor the
# cause. The daemon was down until the plist was rebuilt by hand.
#
# A rename is atomic: the old plist stays intact and complete until a whole new
# one exists to replace it, so a failed install leaves a working daemon rather
# than a truncated one.
sed -e "s|__USER__|$HOME|g" -e "s|__BIN__|$BIN_DIR/tpd|g" \
    "$REPO/install/$LABEL.plist" > "$PLIST.new"
mv -f "$PLIST.new" "$PLIST"

# Reinstalling over a running agent: unload first, or bootstrap reports
# "service already loaded" and the old binary keeps running.
unload
launchctl bootstrap "gui/$(id -u)" "$PLIST"

sleep 1
if launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
    echo "tpd is running. Logs: ~/.teleport/tpd.err.log"
else
    echo "tpd failed to start — check ~/.teleport/tpd.err.log" >&2
    exit 1
fi

# Claude Code, through its own package manager rather than by hand.
#
# `plugin/` already declares everything this used to copy piecemeal: the
# SessionStart/SessionEnd hooks (hooks/hooks.json), the `/tp` command, the skill
# and the MCP server. Installing the plugin delivers all four at once, and —
# unlike a scattering of `install -m 644` — `claude plugin uninstall` removes
# them again without this script's help.
#
# What that replaces: three hand-copies, a 76-line Python file merging JSON into
# the user's settings.json, and a cache-refresh step that existed only because
# the plugin system was being bypassed. It also closes the gap this arrangement
# had all along, which is that install.sh never registered the MCP server at
# all — that key lives in the plugin manifest and cannot be expressed by copying
# a file.
#
# Guarded like the pi block below: teleport supports four runtimes and a machine
# using only codex has no `claude` binary. Not fatal if it fails — but said out
# loud, because without the hooks a session cannot be woken, and that is the
# half of teleport people notice missing.
if command -v claude >/dev/null 2>&1; then
    # add and install are idempotent (both exit 0 saying "already"), and the
    # two updates are what a re-run is FOR: a marketplace sourced from a
    # directory is snapshotted at install time, so editing this repo does not
    # reach the cache until it is refreshed. That gap shipped a stale skill
    # three separate times, and this machine was still running plugin 0.1.0
    # against a 0.2.0 tree when the sequence below was first run.
    if claude plugin marketplace add "$REPO" >/dev/null 2>&1 \
       && claude plugin install teleport@teleport --scope user >/dev/null 2>&1 \
       && claude plugin marketplace update teleport >/dev/null 2>&1 \
       && claude plugin update teleport@teleport >/dev/null 2>&1; then
        echo "Claude Code plugin installed — hooks, /tp, skill and MCP tools"
        # Migrate off the old mechanism. Before the plugin carried hooks,
        # install.sh wrote them straight into the user's settings.json; those
        # entries still work, so leaving them means every session registers
        # twice for no benefit. Removing them is safe and idempotent — the
        # script matches on the exact command string it would have written.
        if [ -f "$REPO/install/setup-hooks.py" ] && command -v python3 >/dev/null 2>&1; then
            python3 "$REPO/install/setup-hooks.py" --uninstall >/dev/null 2>&1 || true
        fi
        rm -f "$HOME/.claude/commands/tp.md"   # the plugin supplies /tp now
    else
        echo "  warning: could not install the Claude Code plugin." >&2
        echo "           tp and tpd work; Claude Code sessions will not be" >&2
        echo "           registerable or reachable until it is installed." >&2
        echo "           Retry:  claude plugin marketplace add $REPO" >&2
        echo "                   claude plugin install teleport@teleport" >&2
    fi
else
    echo "  note: claude not found — skipping the Claude Code plugin."
fi

# Same, for the Pi agent harness (docs/pi-integration.md) — only if pi is
# actually installed; most machines running this repo won't have it.
if [[ -d "$HOME/.pi/agent" ]]; then
    mkdir -p "$HOME/.pi/agent/extensions"
    install -m 644 "$REPO/integrations/pi/teleport.ts" "$HOME/.pi/agent/extensions/teleport.ts"
    # The extension registers the teleport_* tools. The skill that says when to
    # reach for them is NOT installed here any more — it goes to
    # `~/.agents/skills` below, which pi scans alongside its own root, so one
    # file serves pi, codex and dsh instead of three copies drifting apart.
    echo "pi extension installed — run /reload in any already-running pi session to pick it up"
fi

# The skill, once, in the root three of the four runtimes agree to read.
#
# `~/.agents/skills` is not teleport's invention: codex documents it as its User
# scope, dsh calls it "the shared agent config root scanned for compatible
# skills", and pi scans it alongside its own. One file therefore reaches pi,
# codex and dsh — the extension or hook gives a session the teleport_* tools, and
# this is what tells the model when to reach for them. Ship only the tools and
# the capability is present but undiscoverable, which is what shipping only the
# pi extension actually did.
#
# Claude Code is the one that does not scan it (personal, project and plugin
# roots only), and gets the same file through the plugin instead.
mkdir -p "$HOME/.agents/skills/teleport"
install -m 644 "$REPO/plugin/skills/teleport/SKILL.md" \
    "$HOME/.agents/skills/teleport/SKILL.md"
echo "teleport skill installed to ~/.agents/skills — pi, codex and dsh all read it"

# Teleport Panel (docs/teleport-panel-design.md) — menu bar GUI. Optional: it
# needs a Swift toolchain, and teleport is fully usable without it.
#
# A FAILURE HERE IS REPORTED. It used to be `make install && echo installed`,
# where the `&&` guarded only the echo — so a build error or an unwritable
# /Applications left the script printing its success summary while the only
# visible symptom, later, was a menu bar with no icon and no way to know why.
# "Optional" means teleport still works, not that a failure goes unmentioned.
# The directory test comes first and is NOT a failure: a release archive ships
# built binaries, not Swift sources, so `panel/` being absent there is the
# expected shape rather than something that went wrong. Warning about it would
# make the warning meaningless — which is the same reason an empty scan does not
# report itself as degraded.
if [ ! -d "$REPO/panel" ]; then
    :
elif command -v swift >/dev/null 2>&1; then
    if (cd "$REPO/panel" && make install); then
        echo "Teleport Panel installed to /Applications"

        # It is a plain .app, NOT a LaunchAgent like tpd — nothing starts it at
        # login unless it is registered, so after a reboot the icon is simply
        # gone and the app looks broken rather than absent. Asked once, never
        # assumed: adding a login item changes the user's session, and this
        # script should not do that quietly.
        if osascript -e 'tell application "System Events" to get name of every login item' 2>/dev/null | grep -q TeleportPanel; then
            echo "  (already a login item)"
        elif [ -t 0 ]; then
            printf "  Start Teleport Panel automatically at login? [y/N] "
            read -r reply
            case "$reply" in
                [yY]*)
                    osascript -e 'tell application "System Events" to make login item at end with properties {path:"/Applications/TeleportPanel.app", hidden:false}' >/dev/null \
                        && echo "  added to login items" \
                        || echo "  could not add the login item — System Settings > General > Login Items"
                    ;;
                *) echo "  skipped — add it later in System Settings > General > Login Items" ;;
            esac
        else
            echo "  not running interactively; add it in System Settings > General > Login Items to start it at login"
        fi

        open -a /Applications/TeleportPanel.app 2>/dev/null || true
    else
        echo "WARNING: Teleport Panel failed to build or install."
        echo "         Everything else is installed and working; this is the menu bar GUI only."
        echo "         Reproduce with: (cd $REPO/panel && make install)"
    fi
else
    echo "note: no Swift toolchain — skipping the menu bar panel (everything else is installed)."
fi

echo
echo "What takes effect when:"
echo "  tp binary → CLI and pi           : now"
echo "  tp binary → Claude Code MCP tools: NEW Claude Code session (the MCP"
echo "                                     server keeps the binary it started with)"
echo "  skills / tp.md / pi extension    : /reload-skills here, /reload in pi"
echo "  runtimes.d/*.toml                : next tp/tpd invocation (read per-run)"
echo "  tpd                              : restarted above"
echo
echo "  $BIN_DIR/tp id        # this machine's fingerprint"
echo "  $BIN_DIR/tp discover  # find peers on the LAN"
echo
echo "Add $BIN_DIR to PATH if it isn't already."
