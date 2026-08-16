+++
title = "My Mac taught my new Linux box my dev setup — one agent on each, talking over a relay"
date = 2026-08-07
description = "A case study in cross-machine peer agents: one agent on my Mac that knew my years-old dev setup, one on a fresh Arch box that knew Linux, connected over the same encrypted relay my phone uses. Neither could do the job alone. Together they didn't copy my config — they translated it, zsh to fish, brew to pacman, and caught bugs in the original on the way."
+++

*I have a Mac with a dev environment I've tuned for years, and a fresh Linux box I wanted to feel the same. Instead of copying dotfiles or using a dotfile management tool, I started an agent on each machine, connected them as peers over an encrypted relay, and asked them to figure it out and configure my new Linux box. They didn't copy my config, they translated it — and found bugs in the original on the way.*

## The chore I didn't want to do

My work laptop is a Mac, and over years it's accreted a dev environment I actually like: zsh with a pile of aliases, starship, Alacritty in Kanagawa Wave at a font only I would pick, a tmux config, a hand-built Neovim config. Then I built a personal machine — CachyOS, an Arch-based distro, for side projects and games — and I wanted it to feel like home the moment I opened a terminal.

The usual way to do this is a bad afternoon: copy dotfiles over, watch half of them break because Linux isn't macOS, translate the broken half by hand. Also my Mac config is scattered, so it's very likely I'd forget to port over certain things. Even if I was very methodical in configuring my Mac, there are tedious things like: `pacman` is not `brew`; my shell config is zsh but the new box defaults to fish; paths hardcoded to `/Users/hongtaoyang` mean nothing in CachyOS; `pbcopy` doesn't exist on Wayland. It's not hard — it's just tedious work, a hundred small "what's the Arch equivalent of this" decisions.

So I didn't do it. I opened [nudge](https://github.com/nuudge/nudge) — my from-scratch coding agent — on both machines, connected the two sessions as peers, and let the agents do the port between themselves.

## The setup

Two sessions, two machines, one relationship:

- **The Mac agent** could read my real config but couldn't touch Linux. It became the *source* — a read-only surveyor of a setup it happened to be living inside.
- **The Linux agent** knew Arch, `pacman`, `paru`, fish, and Wayland/COSMIC, but had no way to see my Mac. It became the *driver* — the one that installs packages, writes files, and decides what the Arch equivalent of each thing is.

That split is the whole point. Neither agent could do this alone. The information the Linux agent needed lived on a different computer, reachable only through an agent that was local to it. So the driver didn't guess at my dotfiles — it *asked the peer that could read them*:

> Please inventory the Mac side and send me back a single structured report with: `~/.zshrc` … flag which parts are machine/work-specific so I can skip them on this personal machine … `starship.toml` … Alacritty config … `brew leaves` … which Nerd Font (exact family name) … Don't change anything on the Mac.

Connecting them was one command. The session being connected *to* sits on the shared encrypted relay (the same blind pipe [my phone uses to drive a session](/drive-a-coding-agent-from-your-phone/)); the other side runs `/connect-peer <code>` and the link is bidirectional. From then on my Mac agent and my Linux agent could message each other across the internet, and I could mostly step away, intervening only when necessary.

## The port, not the copy

What came back wasn't a file dump the Linux agent wrote to disk. It was raw material the two agents translated, decision by decision, into the Arch/fish/Wayland world. A sample of what "port, don't copy" actually meant:

| On the Mac | What the Linux agent did |
|---|---|
| zsh aliases `gs`/`ga`/`pull`… | fish **abbreviations** (they expand inline — strictly better than aliases) |
| 3 brew-installed zsh plugins (autosuggestions, syntax-highlighting) | dropped — fish has both built in |
| `starship.toml` | copied verbatim (the one genuinely cross-platform file) |
| Alacritty theme `import` at `/Users/hongtaoyang/...` | de-absolutized to `~/.config`, theme written locally |
| `option_as_alt` keyboard dance | removed — Alt works natively on Linux |
| font install via brew cask | `otf-comicshanns-nerd` + `ttf-hack-nerd` from Arch repos |
| font size `20` | retuned to `14` for COSMIC's DPI scaling |
| `brew leaves` (50+ formulae) | the ~12 that are actually cross-platform dev tools, via `pacman`/`paru` |
| `pbcopy`/`pbpaste` for Neovim yank | `wl-clipboard`, wired into nvim's Wayland clipboard |

And a whole column got dropped on purpose. The Mac agent flagged the work-specific bits — my GitLab email, the GDK bootstrap lines, `gcloud`, `glab`, the corporate `duo-cli`, a work-only tmux session manager — so the personal machine never inherited my day job. That flagging is judgment the source agent was positioned to make and the driver wasn't; it knew *which* of my configs were work and which were me.

None of this is a `scp`. Every row is a small decision, and the agent that made it was the one with the local context to make it right.

## The part I didn't expect: it lint-passed my dotfiles

The Linux agent asked the Mac agent to survey, not change. But the Mac agent couldn't read my config closely without noticing what was wrong with it — and it had rot I'd never spotted:

- A `csd = opts.cwd` typo in my Telescope config — a key that silently did nothing for who knows how long.
- `vim.highlight.on_yank()`, deprecated out from under me in a newer Neovim.
- `mise activate` running **twice** in my zshrc (the GitLab dev kit had appended a second copy years ago), quietly doubling my shell startup hooks.
- My zsh syntax-highlighting plugin sourced *before* autosuggestions, when upstream is explicit it must come last or it doesn't wrap the widgets it's supposed to.
- TPM cloned in two different directories, working only by coincidence.

The end result: each agent patched the bugs on its own end. A config port turned into a two-machine lint pass I never asked for.

## Where I actually spent my attention

I wasn't idle — but I only answered things that were genuinely mine to decide:

- **A terminal to try.** I said I was open to something new; the Linux agent recommended Ghostty (good Wayland support, runs on macOS too so I could unify later), set it up *alongside* Alacritty as a fallback, and mirrored my theme into both so the comparison was fair. I'm now living in Ghostty.
- **A security tradeoff, explained before I took it.** To run `pacman` unattended it needed passwordless sudo. It didn't just do it — it explained the exact `sudoers` drop-in, the blast radius ("anything running as you can install packages silently"), and how to delete it after. My call.
- **My git identity.** It refused to copy my work email and asked for a personal one. When I asked "what's `credential.helper = store`?" it told me straight — plaintext tokens on disk — and then talked me *out* of needing one at all, since my remotes are SSH.
- **Desktop muscle memory.** New to COSMIC, I asked how to maximize a window and how to close an app; the Linux agent knew (`Super+M`, `Super+Q`), flagged the macOS habits that would trip me, and set Ghostty to launch maximized.

The Mac agent's whole activity never streamed into my Linux session — that would be information bloat. Peers are quiet by design; each agent decides when to message the other across the link. So when I wanted a status check I just asked, and got a summary back.

## None of this is a feature — it's one idea, again

nudge is built around a single thesis: [symmetric agent communications](/an-agent-is-just-another-client/). Every session is both a thing others attach to and a thing that attaches to others; reaching one is always the same operation — `attach → Controller`, observe what it does, send it input — whether the transport underneath is an in-process channel, a Unix socket, or an encrypted relay across the internet.

Get that right once and the capabilities that other stacks build as separate subsystems all collapse into the same mechanism pointed different directions. [Driving a session from my phone](/drive-a-coding-agent-from-your-phone/) is an attach over the relay. Several clients co-op on one session is the broker fanning to N attaches. A [supervised subagent](/i-didnt-build-an-auto-mode/) is spawn-plus-mutual-attach, a hierarchy with one human at the top. And this post is the fourth shape: **peers** — two independent sessions, each with its own human, laterally connected. Remote control, multi-attach, subagent, peer. One mechanism, four directions. I didn't build a cross-machine-config-porting feature; I ran `/connect-peer`, and the agents used the one `MessagePeer` tool over the exact relay my phone already uses.

Subagent and peer are the two multi-agent shapes, and the difference is what you buy. A subagent gives you a *conclusion*: delegate a task, get a result. A peer gives you *context*: an agent that knows a place you can't reach. My Linux agent didn't want the Mac agent to *do* anything — it wanted what the Mac agent *knew*. That's the whole transaction, and it's exactly what a peer is for: expertise rooted in another machine.

And it stays rooted, because symmetry doesn't mean fusion. Permissions never crossed the link: the Mac agent's reads were gated on the Mac, the Linux agent's installs on Linux, and neither could approve — or even see — the other's prompts. Two directed edges over one relay, each still fully under the control of its own side.

That's the symmetry test passing about as hard as I can make it. Two machines. Two operating systems. Two agent loops that never once branched on `if peer`. And the thing on either end of the link — a human typing at me, or an agent on another continent asking for my starship config — was, to the loop, indistinguishable. There was no difference to tell.

## What this looks like in the code

"An agent is just another client" is easier said than done.

Any front-end reaches a session through one trait and gets back one struct:

```rust
pub trait SessionHandle {
    fn attach(&self, who: ClientIdentity)
        -> impl Future<Output = Option<Controller>> + Send;
    fn detach(&self);
}

pub struct Controller {
    pub events: mpsc::UnboundedReceiver<ControllerEvent>, // observe: what the agent emits
    pub ui_tx:  mpsc::Sender<UiEvent>,                    // drive:   what you send it
}
```

Two half-channels: `events` to watch, `ui_tx` to steer.

Every attach carries an identity, and it isn't decoration:

```rust
pub struct ClientIdentity {
    pub kind: ClientKind,        // Human or Agent
    pub name: String,
    pub session_id: Option<String>,
    pub task: Option<String>,
}
```

That's why my Mac agent showed up in the Linux session as a *named* peer rather than an anonymous socket.

The set of peers an agent holds is a bag of `Controller`s — each peer is held as an ordinary `Controller`, the exact type a human front-end gets from `attach`, so `observe` = drain its events and `drive` = send on its `ui_tx`.

So when my Linux agent's model called `MessagePeer` to ask the Mac side for my zshrc, the *entire* implementation of "send a message to a peer" is this:

```rust
peers.drive(id, UiEvent::UserMessage { text: message.to_string() }).await;
Ok(format!("message sent to {peer}"))
```

And `drive` itself is one line — `p.controller.ui_tx.send(ev).await` — pushing a `UserMessage` up the peer's drive channel. That is the same event I produce when I type into a session. No "peer message" type, no inbox, no routing table: a message from an agent and a message from a human are the same `UiEvent::UserMessage` on the same `ui_tx`, and the receiving loop can't tell them apart. The `MessagePeer` tool's own description puts it as "the exact path a human's message takes."

When the link had dropped and the model addressed a peer that was gone, the error it hit came from these lines:

```rust
let Some(id) = peers.find_by_name(peer) else {
    let roster = peers.roster();
    bail!("no peer named '{peer}'; current peers: {}", roster.join(", "));
};
```

That string — `no peer named 'ab370f6f'; current peers: mac-config-server` — is exactly what my Linux agent hit when the relay dropped mid-port (more on that below). It isn't a special peer-failure path; it's a tool failing to resolve a name, and the agent reading the failure and re-addressing the peer under its new handle. The recovery was just the model doing what it does with any tool error.

The only thing that made this cross-machine rather than in-process is the transport beneath `attach`. A phone attaches over the relay with an ordinary one-way frame; a peer sends the same frame with one bit flipped:

```rust
Attach {
    after_seq: Option<u64>,
    who: ClientIdentity,
    #[serde(default, skip_serializing_if = "is_false")]
    reverse_offer: bool,   // "also let the far side drive me back"
}
```

`reverse_offer` asks the acceptor to open the return edge, so both directed edges of the peer relationship ride one duplex socket (`ReverseCommand` carries the acceptor's drives back, `ReverseEvent` its events). When it's false — every phone, every ordinary client — the frame serializes byte-for-byte as before, which is why the same relay, the same encryption, and the same Kotlin phone client kept working untouched. Cross-machine peering wasn't a new transport. It is the same transport with one optional bit.

## The honest limits

There are still some rough edges.

- **The link dropped. Twice.** Mid-port, the relay connection died, and each reconnect handed the peer a *new* name — my Linux agent went looking for `ab370f6f` and got told "no peer named that; current peers: `mac-config-server`." It recovered fine (re-addressed the new handle, asked for a resend, and the Mac agent re-sent its whole inventory split into four parts), but there's no auto-reconnect yet — a human has to re-run `/connect-peer`. It's a known gap, and on a flaky relay leg you feel it. I'll spend time closing this gap later.

- **Both agents were mine.** The canonical peer use case is you and a *colleague*, each running your own session. Here both machines were mine, so I was the human on both ends, hopping between them. That doesn't change the mechanism — each agent was still the local expert of its own box, still gated on its own side — but I want to be straight that I didn't stress-test the two-humans (you and your colleague) case here.

Neither is a hole in the idea. One's a reconnection feature; the other's just the next thing to try.

## Where this lands

I set out to do a boring afternoon of dotfile surgery. Instead, two agents ported my dotfiles across two machines and two operating systems, over an encrypted relay. I let the one that knew Arch interview the one that knew my setup. What I got back wasn't a copy — it was a translation, plus a free audit of the original.

---

*nudge is an open-source coding agent built from scratch in Rust. [Browse the code](https://github.com/nuudge/nudge), or read the design that made this free in [An agent is just another client](/an-agent-is-just-another-client/).*
