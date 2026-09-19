//! Streaming markdown for display: closing unterminated syntax as it arrives.
//!
//! Language models emit markdown. Mid-stream a partial document renders badly:
//! `**bold` shows two literal asterisks, an unterminated ` ``` ` fence swallows
//! every subsequent token as code, `[text](http` confuses the link parser.
//! [`mdstitch`] closes those markers so each intermediate frame is well-formed
//! CommonMark.
//!
//! ## The rule this module exists to enforce
//!
//! **Stitched text is for display only and must never reach the session log.**
//!
//! That is not a stylistic preference, it is a correctness invariant with three
//! teeth:
//!
//! 1. **The log must record what the model said.** A stitched
//!    `**bold**` where the model wrote `**bold` is a fabricated byte. Replaying
//!    that log would produce a conversation the model never had, and the
//!    divergence would be invisible because both render identically.
//! 2. **Stitching is not idempotent under truncation.** `stitch` closes an open
//!    marker at the *end of the buffer*, so a later delta extends the text and
//!    the earlier "closed" form is superseded. Recording it would persist a
//!    boundary that the model never wrote and that the next token contradicts.
//! 3. **A structured response is not markdown at all.** Tool-call arguments and
//!    JSON-mode output are JSON; running a markdown repairer over them could
//!    mutate a document a client will parse. The format is chosen per block.
//!
//! So this module is called at exactly one place — the point where accumulated
//! text is about to be shown to a client — and it returns a value that is never
//! appended to a log. [`StitchedProse`] makes that hard to get wrong by being a
//! distinct type from anything the log writer accepts.
//!
//! ## Why the accumulator is here rather than in the client
//!
//! The client already accumulates `text-delta` chunks into blocks (upstream's
//! `PartialAccumulator`). Stitching the *accumulation* rather than each delta is
//! the whole point: a repairer run on a lone fragment cannot tell `**bold` from
//! a complete `**bold**` split across two frames. So the full text of a block is
//! what gets stitched, once, at the moment the client would render it.
//!
//! ## Which blocks are prose
//!
//! [`BlockKind`] is the discriminator, and it is deliberately explicit: text is
//! stitched, reasoning is stitched (models emit markdown there too), and
//! **tool-call arguments are not** — they are JSON, and a markdown repairer
//! over JSON is a bug waiting to be reported as corrupt arguments.

use std::borrow::Cow;
use std::collections::BTreeMap;

use mdstitch::{StitchOptions, open_fence, stitch};

use super::llm_replay::BlockType;

/// A block kind, as the *client's* rendering decision sees it.
///
/// Distinct from [`BlockType`] only in that it exists to answer "is this prose",
/// which is a display question rather than a wire one. Keeping the mapping in
/// one place is what stops a future block kind from silently defaulting to
/// "stitch it" — the default that would corrupt JSON.
pub fn is_prose(block_type: BlockType) -> bool {
    match block_type {
        // Models write markdown in both of these.
        BlockType::Text | BlockType::Reasoning => true,
        // Tool arguments are JSON, not prose. A repairer here would mutate a
        // document the client parses.
        BlockType::ToolCall => false,
    }
}

/// Text that has been stitched for display.
///
/// A distinct type on purpose: the session log's row writers take `&str`, so a
/// value of this type cannot be passed where log content is expected without an
/// explicit `.as_str()` — which is the moment a reviewer sees the mistake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StitchedProse(String);

impl StitchedProse {
    /// The stitched text, for display.
    ///
    /// Test-only: the live path consumes the value with [`Self::into_string`],
    /// and a borrow here would only be used to assert on stitched output. Kept
    /// rather than deleted because it is what makes the repair assertions read
    /// as comparisons against `&str` instead of against a wrapper.
    #[cfg(test)]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the display string.
    pub fn into_string(self) -> String {
        self.0
    }
}

/// The options this host stitches with.
///
/// [`StitchOptions::default`] enables every repairer, which is right for prose
/// streamed token-by-token. One deliberate departure: `inline_katex` stays off
/// because a single `$` is ambiguous with currency — the crate's own default,
/// kept here explicitly so a future "enable everything" edit has to argue with
/// a comment rather than a bare default.
pub fn options() -> StitchOptions {
    StitchOptions::default()
}

/// Stitch accumulated prose for display.
///
/// Returns `Cow::Borrowed` when nothing needed closing, which is the crate's
/// zero-allocation fast path — plain prose, or a block whose markdown is
/// already balanced, copies nothing.
///
/// An empty block yields an empty string rather than an error: a block that has
/// not streamed its first byte yet is a normal state, not a fault.
#[cfg(test)]
pub fn stitch_prose<'a>(text: &'a str, opts: &StitchOptions) -> Cow<'a, str> {
    stitch(text, opts)
}

/// Append the closer for an unterminated code fence, if one is open.
///
/// The crate does **not** close fences itself. It exposes
/// [`open_fence`], which reports the opening run's character and length, and
/// leaves the closer to the caller — because the closer must match the opener:
/// a four-backtick fence is not closed by three, and a tilde fence is not closed
/// by backticks at all. A blanket `\n\`\`\`` would end the block with a *different*
/// fence than it began with, and CommonMark would keep the code block open
/// through the rest of the document.
///
/// So this is the half of the repair the caller owes, and it is the reason a
/// stitched fence test is worth having: the crate's headline list mentions
/// fences, but the repair is split across the boundary by design.
pub fn close_open_fence(text: &str) -> Cow<'_, str> {
    let Some((open_char, run)) = open_fence(text) else {
        return Cow::Borrowed(text);
    };
    let mut out = String::with_capacity(text.len() + run + 2);
    out.push_str(text);
    // The fence must start on its own line to close the block.
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.extend(std::iter::repeat_n(open_char, run));
    out.push('\n');
    Cow::Owned(out)
}

/// Stitch a block's accumulated text, if it is prose at all.
///
/// The single entry point callers should use: it applies the prose decision, the
/// crate's repairer, and the fence closer together, so a caller cannot stitch
/// JSON by reaching for the repairer directly without also reaching past
/// [`is_prose`].
pub fn stitch_block(text: &str, block_type: BlockType, opts: &StitchOptions) -> StitchedProse {
    if !is_prose(block_type) {
        return StitchedProse(text.to_string());
    }
    // Order matters: the crate repairs inline markers, then the fence is
    // closed. Doing it the other way around would let the crate's inline
    // handling run over text that now ends inside a fence it just closed.
    let stitched = stitch(text, opts);
    StitchedProse(close_open_fence(stitched.as_ref()).into_owned())
}

/// The accumulated text of each block in a streaming response, with the
/// **incremental display delta** each new fragment implies.
///
/// This mirrors what a client does when it folds `text-delta` chunks: text is
/// concatenated per block index and rendered as one document. Two properties
/// have to hold at once, and they pull in opposite directions:
///
/// 1. The text a client *ends up with* must be stitched — `**bo` on its own
///    renders as two literal asterisks.
/// 2. Each frame must carry only the *new* text, because the protocol's
///    `text-delta` is append-only: a client accumulates with
///    `prev + chunk.text`, so sending the whole stitching on every frame makes
///    the rendered answer grow quadratically — the model's first word repeated
///    once per token that follows it.
///
/// A live run is what showed the second property: the first implementation
/// emitted the full stitching per frame, and the frames looked correct
/// individually while rendering as garbage in aggregate.
///
/// So the accumulator keeps, per block, both the text it has and the prefix of
/// that text a client has already been sent. A new delta is whatever the
/// stitched form now ends with beyond that prefix — and because stitching can
/// *rewrite* the tail (closing `**bo` to `**bo**` appends rather than changes
/// the prefix, but a fence closer inserts a newline before it), the comparison
/// is against the previously-sent string rather than an offset into the raw
/// text.
///
/// Tool-call blocks are carried through **unstitched**, and their fragments are
/// still concatenated, because a client assembling arguments needs the raw JSON
/// exactly as the model emitted it.
/// `StitchOptions` is not `Clone` (it can hold boxed handlers), so this type is
/// not either — an accumulator owns its options for its lifetime, which is what
/// a per-response value wants anyway.
#[derive(Debug)]
pub struct DisplayAccumulator {
    blocks: BTreeMap<u64, BlockState>,
    opts: StitchOptions,
}

/// One block's text, and how much of its *display* form has been sent.
#[derive(Debug)]
struct BlockState {
    block_type: BlockType,
    /// What the model wrote, unstitched.
    raw: String,
    /// What a client has already been sent for this block, in display form.
    ///
    /// Needed because the delta is a suffix of the *stitched* text and the
    /// stitched text is recomputed from the whole raw buffer each time: without
    /// the previously-sent value there is no way to tell which part of the new
    /// stitching is new.
    sent: String,
}

impl DisplayAccumulator {
    pub fn new() -> Self {
        Self {
            blocks: BTreeMap::new(),
            opts: options(),
        }
    }

    /// Note a block's kind as soon as it opens.
    ///
    /// Recorded at open rather than inferred from the first delta, because a
    /// tool call whose arguments have not streamed yet would otherwise have no
    /// kind, and guessing "text" there is exactly the mistake that corrupts
    /// JSON.
    pub fn open(&mut self, index: u64, block_type: BlockType) {
        self.blocks.entry(index).or_insert(BlockState {
            block_type,
            raw: String::new(),
            sent: String::new(),
        });
    }

    /// Append a text or reasoning fragment and return the display delta it
    /// implies.
    ///
    /// The returned string is what a client should be sent as this frame's
    /// `text`; concatenating every returned value reproduces the stitched text.
    /// A fragment that adds nothing displayable (a lone whitespace run inside an
    /// already-open marker, say) returns `None` rather than an empty string, so
    /// the caller can skip the frame entirely instead of sending a no-op.
    pub fn push_delta(&mut self, index: u64, block_type: BlockType, text: &str) -> Option<String> {
        let state = self.blocks.entry(index).or_insert(BlockState {
            block_type,
            raw: String::new(),
            sent: String::new(),
        });
        state.block_type = block_type;
        state.raw.push_str(text);
        if !is_prose(block_type) {
            // Not markdown: the fragment is forwarded verbatim, so the delta is
            // the fragment. Stitching JSON is the failure `is_prose` prevents.
            state.sent.push_str(text);
            return Some(text.to_string());
        }
        let display = stitch_block(&state.raw, block_type, &self.opts).into_string();
        // The delta is what the display form has beyond what was already sent.
        // It is *not* simply the tail of the raw buffer: closing a marker
        // appends characters the model has not written yet, and those are
        // exactly the characters the client needs to render this frame.
        if display.len() <= state.sent.len() {
            // The stitch can also *shorten* nothing (it only appends or
            // borrows), so this means the display form did not grow.
            return None;
        }
        let delta = display[state.sent.len()..].to_string();
        state.sent = display;
        Some(delta)
    }

    /// Append a text or reasoning fragment, discarding the delta.
    ///
    /// For callers that only want the accumulated state — a test, or a path
    /// where nothing is being displayed.
    #[cfg(test)]
    pub fn push_text(&mut self, index: u64, block_type: BlockType, text: &str) {
        let _ = self.push_delta(index, block_type, text);
    }

    /// Whether any block has content yet.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.blocks.values().all(|b| b.raw.is_empty())
    }

    /// The stitched display text of one block.
    ///
    /// Not test-only: this is what a `block-end` payload carries, and `block-end`
    /// is the one point on this wire where a retraction makes stitched text
    /// legal.
    pub fn display(&self, index: u64) -> Option<StitchedProse> {
        let state = self.blocks.get(&index)?;
        Some(stitch_block(&state.raw, state.block_type, &self.opts))
    }

    /// Every block's display text, in block order.
    ///
    /// Ordered by index rather than insertion because that is the order a
    /// client renders blocks in — reasoning before the text it precedes — and a
    /// BTreeMap gives that for free.
    #[cfg(test)]
    pub fn display_all(&self) -> Vec<(u64, StitchedProse)> {
        self.blocks
            .iter()
            .map(|(index, state)| {
                (
                    *index,
                    stitch_block(&state.raw, state.block_type, &self.opts),
                )
            })
            .collect()
    }

    /// The raw accumulated text of a block, unstitched.
    ///
    /// For a caller that needs what the model actually wrote. The live path
    /// does not: the durable record is built from the buffered decode, not from
    /// this accumulator, precisely so display state can never reach the log.
    #[cfg(test)]
    pub fn raw(&self, index: u64) -> Option<&str> {
        self.blocks.get(&index).map(|b| b.raw.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The prose decision is the load-bearing safety property, so it is
    /// asserted for every block kind rather than whichever one a test happens
    /// to exercise.
    #[test]
    fn tool_arguments_are_never_stitched() {
        assert!(is_prose(BlockType::Text));
        assert!(is_prose(BlockType::Reasoning));
        assert!(
            !is_prose(BlockType::ToolCall),
            "tool arguments are JSON; a markdown repairer over them corrupts them"
        );
    }

    /// A JSON argument string containing markdown-looking characters must come
    /// back byte-identical. This is the failure the prose check prevents.
    #[test]
    fn a_tool_argument_with_markdown_lookalikes_survives_unchanged() {
        // A JSON argument whose *string value* holds an unterminated bold
        // marker and an open fence: a repairer would close both and hand the
        // client invalid JSON.
        let args = r#"{"body":"**unclosed and ```fence"}"#;
        let out = stitch_block(args, BlockType::ToolCall, &options());
        assert_eq!(out.as_str(), args, "JSON arguments must not be rewritten");
    }

    /// Text is stitched: the crate's own headline example.
    #[test]
    fn incomplete_bold_is_closed_for_display() {
        let out = stitch_block("Hello **wor", BlockType::Text, &options());
        assert_eq!(out.as_str(), "Hello **wor**");
    }

    /// Reasoning is stitched too: models write markdown there, and a client
    /// renders it through the same markdown path.
    #[test]
    fn reasoning_is_stitched_like_text() {
        let out = stitch_block("thinking about `code", BlockType::Reasoning, &options());
        assert_eq!(out.as_str(), "thinking about `code`");
    }

    /// The stitching is display-only; the raw text keeps what the model wrote.
    ///
    /// This is the invariant the whole module is arranged around: a value that
    /// goes into the session log must be byte-identical to the model's output.
    #[test]
    fn the_raw_text_is_what_the_model_wrote() {
        let mut acc = DisplayAccumulator::new();
        acc.open(0, BlockType::Text);
        acc.push_text(0, BlockType::Text, "Hello **wor");
        assert_eq!(
            acc.raw(0),
            Some("Hello **wor"),
            "the log keeps the model's bytes"
        );
        assert_eq!(acc.display(0).unwrap().as_str(), "Hello **wor**");
    }

    /// A block that opens before any delta arrives is reported with its kind,
    /// so a later tool fragment is not mistaken for text.
    #[test]
    fn a_block_keeps_the_kind_it_was_opened_with() {
        let mut acc = DisplayAccumulator::new();
        acc.open(0, BlockType::ToolCall);
        acc.push_text(0, BlockType::ToolCall, r#"{"a":**"#);
        // The kind recorded at open survives, so this stays JSON.
        assert_eq!(acc.raw(0), Some(r#"{"a":**"#));
        assert_eq!(
            acc.display(0).unwrap().as_str(),
            r#"{"a":**"#,
            "a tool block must not be repaired even mid-fragment"
        );
    }

    /// An open fence swallows every subsequent token as code, which is why this
    /// matters beyond cosmetics.
    ///
    /// The closer carries a trailing newline so the fence ends on its own line —
    /// otherwise the next streamed token would land on the closer's line and be
    /// read as part of it.
    #[test]
    fn an_unterminated_fence_is_closed() {
        let out = stitch_block("Here:\n```rust\nlet x = 1;\n", BlockType::Text, &options());
        assert_eq!(out.as_str(), "Here:\n```rust\nlet x = 1;\n```\n");
    }

    /// Block order is by index, not insertion: a client renders reasoning
    /// before the text it precedes even if the text delta arrived first.
    #[test]
    fn blocks_are_ordered_by_index() {
        let mut acc = DisplayAccumulator::new();
        acc.push_text(1, BlockType::Text, "answer");
        acc.push_text(0, BlockType::Reasoning, "thinking");
        let all: Vec<u64> = acc.display_all().into_iter().map(|(i, _)| i).collect();
        assert_eq!(all, vec![0, 1]);
    }

    /// Plain prose takes the crate's borrowed fast path — no copy when nothing
    /// needs closing, which is the common case for a completed block.
    #[test]
    fn balanced_markdown_is_borrowed_not_copied() {
        let text = "**bold** and `code` and [a](b)";
        assert!(
            matches!(stitch_prose(text, &options()), Cow::Borrowed(_)),
            "a balanced document must not be reallocated"
        );
    }

    /// A fence is closed with a closer matching its opener.
    ///
    /// The run length matters: CommonMark closes a four-backtick fence only with
    /// four or more, so a blanket three-backtick closer would leave the block
    /// open — the opposite of the intent. This is the half of the repair the
    /// crate leaves to the caller (it exposes `open_fence` rather than closing),
    /// which is why it is tested against the longer run specifically.
    #[test]
    fn a_fence_is_closed_with_a_matching_run() {
        // Four backticks: a three-backtick closer would not end this block.
        let text = "````md\ncode\n";
        let out = stitch_block(text, BlockType::Text, &options());
        assert_eq!(out.as_str(), "````md\ncode\n````\n");

        // A tilde fence closes with tildes.
        let text = "~~~py\ncode\n";
        let out = stitch_block(text, BlockType::Text, &options());
        assert_eq!(out.as_str(), "~~~py\ncode\n~~~\n");
    }

    /// A closed fence is left alone, and so is text with no fence at all.
    #[test]
    fn a_closed_fence_is_not_double_closed() {
        let text = "```\ncode\n```\n";
        assert_eq!(
            stitch_block(text, BlockType::Text, &options()).as_str(),
            text,
            "a closed fence must not gain a second closer"
        );
        let plain = "no fence here\n";
        assert_eq!(
            stitch_block(plain, BlockType::Text, &options()).as_str(),
            plain
        );
    }

    /// Stitching is for a *partial* document. A complete one must be unchanged,
    /// or display would differ from the durable text for finished turns.
    #[test]
    fn a_complete_document_is_unchanged() {
        let text = "# Title\n\nSome **bold**, some `code`.\n\n```js\nlet a = 1\n```\n";
        assert_eq!(
            stitch_block(text, BlockType::Text, &options()).as_str(),
            text,
            "a finished document must round-trip untouched"
        );
    }

    /// An empty block is a normal state (no delta yet), not a fault.
    #[test]
    fn an_empty_block_is_empty_not_an_error() {
        let mut acc = DisplayAccumulator::new();
        acc.open(0, BlockType::Text);
        assert!(acc.is_empty());
        assert_eq!(acc.display(0).unwrap().as_str(), "");
    }

    /// **Stitched deltas cannot ride in an append-only field**, and this test is
    /// the proof rather than a regression guard.
    ///
    /// `text-delta.text` is append-only — a client accumulates `prev + text` —
    /// and stitching *inserts* characters the model never wrote. Closing
    /// `**bo` to `**bo**` appends two asterisks before the answer continues, so
    /// the closer is baked into the concatenation and every later fragment
    /// lands after it. Concatenating stitched deltas gives
    /// `Here is **bo**** text` where the model wrote `Here is **bold** text`.
    ///
    /// So no sequence of per-frame *stitched* deltas can sum to the stitched
    /// text, and the repair has to happen where the whole text is known: the
    /// client's accumulated buffer, or a retraction point like `block-end`
    /// (which the client applies wholesale). This test pins the impossibility
    /// so a future "let's just stitch each delta" change fails here instead of
    /// shipping garbled answers that look fine frame by frame.
    #[test]
    fn stitched_deltas_cannot_sum_to_the_stitched_text() {
        let mut acc = DisplayAccumulator::new();
        acc.open(0, BlockType::Text);
        let mut sent = String::new();
        for fragment in ["Here ", "is **bo", "ld** done"] {
            if let Some(delta) = acc.push_delta(0, BlockType::Text, fragment) {
                sent.push_str(&delta);
            }
        }
        // The concatenation of the deltas is NOT the stitched text — the
        // inserted closer survives into the sum.
        assert_ne!(
            sent,
            acc.display(0).unwrap().as_str(),
            "if these ever match, the append-only constraint has changed and \
             stitching deltas became legal — re-derive the design, do not just \
             delete this test"
        );
        assert_eq!(
            sent, "Here is **bo**** done",
            "the inserted closer is baked into the sum: {sent:?}"
        );
        // What *is* stitched, and what a retraction point must therefore carry,
        // is the whole-block form.
        assert_eq!(acc.display(0).unwrap().as_str(), "Here is **bold** done");
    }

    /// The stitched form of a *complete* block is the whole answer, which is
    /// what a `block-end` payload carries.
    #[test]
    fn the_accumulation_is_stitched_as_a_whole() {
        let mut acc = DisplayAccumulator::new();
        acc.open(0, BlockType::Text);
        for fragment in ["```rust\n", "let x = 1;\n"] {
            acc.push_delta(0, BlockType::Text, fragment);
        }
        // Mid-fence: the closer is present, so a client that rendered this
        // frame sees a closed block rather than the rest of the answer eaten
        // as code.
        assert_eq!(
            acc.display(0).unwrap().as_str(),
            "```rust\nlet x = 1;\n```\n"
        );
        // Once the model closes it, the stitched form is exactly what it wrote.
        acc.push_delta(0, BlockType::Text, "```\n");
        assert_eq!(
            acc.display(0).unwrap().as_str(),
            "```rust\nlet x = 1;\n```\n"
        );
    }
}
