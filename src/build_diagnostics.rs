//! Turning a failed build's raw stderr into an error a human can act on.
//!
//! Pure and cross-platform so the Linux-only merge gate covers it; the callers
//! in [`crate::build_monitor`] are the ones that touch processes and disks.
//!
//! # The defect this module exists to fix
//!
//! A failed cargo build's stored error was `<matched error lines>` plus the
//! **last 2 KB** of stderr, and nothing else.
//!
//! For the runner's bin crate that is worse than useless. The final `rustc`
//! invocation cargo echoes on failure is itself far longer than 2 KB — it is a
//! wall of `-L`/`--extern`/linker flags — so the 2 KB tail window contained
//! *only* flags, plus the terminal
//! `(exit code: 0xc0000409, STATUS_STACK_BUFFER_OVERRUN)`. The actual cause,
//!
//! ```text
//! memory allocation of 2097152 bytes failed
//! ```
//!
//! was **line 18** — near the top of the log, and matched none of the
//! `BUILD_ERROR_PATTERNS`, so it appeared in neither half of the message. The
//! stored error therefore described a stack-buffer-overrun exit code on a
//! machine that had simply run out of memory. Diagnosis followed the exit code
//! and cost hours (2026-07-31).
//!
//! Two structural lessons drive the design here:
//!
//! 1. **The tail is the least informative part of a build log.** Compiler
//!    diagnostics, allocator aborts, panics and LLVM errors are emitted when
//!    they happen — i.e. EARLY — and are then buried under however much
//!    trailing noise the toolchain prints. A window anchored only at the end
//!    is anchored at the wrong end. So we keep the HEAD as well as the tail.
//! 2. **Position-based windows are a lossy proxy for "the interesting line".**
//!    Even a generous head+tail can miss a cause in the middle of a long log.
//!    So we ALSO scan the whole text for known fatal signatures and hoist them
//!    to the very front of the message, where they cannot be cropped.
//!
//! The two mechanisms are deliberately redundant: the signature scan catches
//! what we anticipated, the head+tail window catches what we did not.

use std::sync::LazyLock;

use regex::Regex;

/// How much of the START of stderr to keep in the operator-facing excerpt.
/// Larger than the tail because this is where causes live.
pub const FAILURE_EXCERPT_HEAD_BYTES: usize = 6 * 1024;

/// How much of the END of stderr to keep. Cargo's terminal
/// "could not compile … (exit code …)" summary and the last diagnostic live
/// here, so it stays worth keeping — just not worth keeping *alone*.
pub const FAILURE_EXCERPT_TAIL_BYTES: usize = 4 * 1024;

/// Maximum number of fatal-signature lines hoisted into the summary. A build
/// that trips hundreds of them (every rustc invocation OOMing, say) should
/// still produce a readable error.
const MAX_SIGNATURES: usize = 12;

/// Longest single signature line kept. Some matches (a linker command echo)
/// are enormous; the point is to name the cause, not to reproduce the line.
const MAX_SIGNATURE_LINE_CHARS: usize = 400;

/// What class of fatal condition a matched line indicates.
///
/// The ORDER of the variants is the ranking used when several signatures are
/// present: a resource exhaustion is the cause of the compiler abort that
/// follows it, so it must be reported first. Getting this backwards is exactly
/// how `STATUS_STACK_BUFFER_OVERRUN` (a *symptom*, ranked
/// [`FatalKind::ProcessAbort`]) outranked `memory allocation … failed` (the
/// *cause*, ranked [`FatalKind::ResourceExhaustion`]) in the incident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FatalKind {
    /// The machine ran out of memory or disk. Causes everything below it.
    ResourceExhaustion,
    /// The toolchain aborted: allocator abort, stack overflow, LLVM fatal
    /// error, a panic, a killed/abnormally-exited child.
    ProcessAbort,
    /// A genuine rustc diagnostic (`error[E####]`) — the user's code.
    CompilerDiagnostic,
    /// Linking failed.
    LinkFailure,
    /// A generic `error:` line from cargo/rustc. Noisiest, so ranked last.
    GenericError,
}

impl FatalKind {
    /// Short label used in the rendered summary.
    pub fn label(&self) -> &'static str {
        match self {
            FatalKind::ResourceExhaustion => "RESOURCE EXHAUSTION",
            FatalKind::ProcessAbort => "ABORT",
            FatalKind::CompilerDiagnostic => "COMPILER",
            FatalKind::LinkFailure => "LINK",
            FatalKind::GenericError => "ERROR",
        }
    }
}

/// One matched fatal line, with where it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FatalSignature {
    pub kind: FatalKind,
    /// 1-based line number within the full stderr — so the reader can find it
    /// in `<slot>/last-build.stderr` even when the excerpt cropped it.
    pub line_no: usize,
    pub text: String,
}

/// The signature table, most-causal first.
///
/// Every pattern here earns its place from an observed failure mode, not from
/// speculation:
///
/// * `memory allocation of N bytes failed` — Rust's allocator abort. THE line
///   the 2 KB tail cropped. On Windows the process then dies with
///   `0xc0000409`, which is what the old message reported instead.
/// * `os error 1455` / `paging file` — Windows commit-limit exhaustion, the
///   same condition wearing a different message.
/// * `No space left` / `not enough space on the disk` — the disk-full analogue,
///   which has taken this fleet down before.
/// * `has overflowed its stack`, `LLVM ERROR`, `fatal runtime error`,
///   `panicked at`, `exited abnormally`, `STATUS_ACCESS_VIOLATION`,
///   `0xc0000409`, `signal: 9` — the toolchain-abort family. None of these
///   match `BUILD_ERROR_PATTERNS`, so before this module a build that died
///   this way stored an error with no cause in it at all.
static SIGNATURE_PATTERNS: LazyLock<Vec<(FatalKind, Regex)>> = LazyLock::new(|| {
    let compile = |k: FatalKind, p: &str| (k, Regex::new(p).expect("static regex compiles"));
    vec![
        compile(
            FatalKind::ResourceExhaustion,
            r"(?i)memory allocation of \d+ bytes failed",
        ),
        compile(
            FatalKind::ResourceExhaustion,
            r"(?i)\b(out of memory|cannot allocate memory|oom-killer|killed process)\b",
        ),
        compile(
            FatalKind::ResourceExhaustion,
            r"(?i)(os error 1455|paging file is too small)",
        ),
        compile(
            FatalKind::ResourceExhaustion,
            r"(?i)(no space left on device|not enough space on the disk|os error 28)",
        ),
        compile(FatalKind::ProcessAbort, r"(?i)has overflowed its stack"),
        compile(FatalKind::ProcessAbort, r"LLVM ERROR"),
        compile(FatalKind::ProcessAbort, r"(?i)fatal runtime error"),
        compile(FatalKind::ProcessAbort, r"panicked at"),
        compile(FatalKind::ProcessAbort, r"(?i)exited abnormally"),
        compile(
            FatalKind::ProcessAbort,
            r"(?i)(STATUS_STACK_BUFFER_OVERRUN|STATUS_ACCESS_VIOLATION|0xc0000409|0xc0000005)",
        ),
        compile(FatalKind::ProcessAbort, r"signal: (9|11|6)\b"),
        compile(FatalKind::CompilerDiagnostic, r"error\[E\d+\]"),
        // The frontend half of the same problem: `tsc`/`vite` print
        // `error TS####` to STDOUT, early, and then pnpm buries it under
        // harness noise. Same renderer, same reason.
        compile(FatalKind::CompilerDiagnostic, r"error TS\d+"),
        compile(
            FatalKind::LinkFailure,
            r"(?i)(error: linking with .* failed|could not exec the linker|undefined reference to|LNK\d{4})",
        ),
        // Deliberately last and deliberately broad: cargo's own failures
        // ("error: could not compile", "error: failed to run custom build
        // command") carry no code, and losing them is how an environmental
        // failure ends up with an empty error body.
        compile(FatalKind::GenericError, r"(?m)^\s*error(:|\s)"),
    ]
});

/// Scan the FULL stderr for known fatal signatures.
///
/// Returns them ranked by [`FatalKind`] (most causal first), then by position,
/// de-duplicated by text and capped at [`MAX_SIGNATURES`]. Scanning the whole
/// text is the point: this is what survives when the cause sits outside any
/// positional window.
pub fn extract_fatal_signatures(stderr: &str) -> Vec<FatalSignature> {
    let mut hits: Vec<FatalSignature> = Vec::new();

    for (idx, raw_line) in stderr.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        // First (most causal) matching pattern wins for a given line, so an
        // OOM line is not also reported as a generic `error:`.
        if let Some((kind, _)) = SIGNATURE_PATTERNS.iter().find(|(_, re)| re.is_match(line)) {
            hits.push(FatalSignature {
                kind: *kind,
                line_no: idx + 1,
                text: truncate_chars(line, MAX_SIGNATURE_LINE_CHARS),
            });
        }
    }

    // Stable sort by causality rank; equal ranks keep source order because
    // `sort_by_key` is stable.
    hits.sort_by_key(|h| h.kind);
    // Real de-duplication, not `dedup_by` — that only collapses ADJACENT
    // equals, so `error: X` / `error: Y` / `error: X` would keep both copies
    // of X and burn two of the MAX_SIGNATURES slots on the same line.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    hits.retain(|h| seen.insert(h.text.clone()));
    hits.truncate(MAX_SIGNATURES);
    hits
}

/// Truncate to at most `max` CHARS (not bytes), so the result is always valid
/// UTF-8 and never splits a multi-byte character.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}… [line truncated]")
}

/// Keep the first `head` bytes AND the last `tail` bytes of `s`, with an
/// explicit marker naming how much was dropped in between.
///
/// The elision marker is not decoration: without it a reader cannot tell a
/// short complete log from a long cropped one, and will draw conclusions from
/// adjacency that the crop invented.
///
/// Byte offsets are snapped to UTF-8 character boundaries.
pub fn head_and_tail(s: &str, head: usize, tail: usize) -> String {
    if s.len() <= head + tail {
        return s.to_string();
    }

    let mut head_end = head;
    while head_end < s.len() && !s.is_char_boundary(head_end) {
        head_end += 1;
    }
    let mut tail_start = s.len().saturating_sub(tail);
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    // Degenerate windows (head+tail >= len after boundary snapping) collapse
    // to the whole string rather than producing a negative elision.
    if tail_start <= head_end {
        return s.to_string();
    }

    let elided = tail_start - head_end;
    let elided_lines = s[head_end..tail_start].lines().count();
    format!(
        "{}\n\n--- [{} bytes / ~{} lines elided; full log in <slot>/last-build.stderr] ---\n\n{}",
        &s[..head_end],
        elided,
        elided_lines,
        &s[tail_start..]
    )
}

/// Render the operator-facing error for a failed build.
///
/// Layout, in priority order:
///
/// 1. `base` — the caller's headline (exit status, or the matched error lines).
/// 2. The fatal signatures, hoisted from ANYWHERE in the log with their line
///    numbers. This is the section that would have said
///    `RESOURCE EXHAUSTION (line 18): memory allocation of 2097152 bytes failed`
///    on 2026-07-31.
/// 3. A head+tail excerpt of the stderr itself, so context survives even for a
///    failure mode we never anticipated.
///
/// Returns `base` unchanged when there is no stderr to add.
pub fn render_build_failure(base: &str, full_stderr: &str) -> String {
    render_output_failure(base, full_stderr, "cargo stderr")
}

/// [`render_build_failure`] with an explicit name for the output stream, so a
/// pnpm/tsc failure is not labelled "cargo stderr". Same renderer either way —
/// the defect and the fix are identical for both toolchains.
pub fn render_output_failure(base: &str, full_output: &str, stream_label: &str) -> String {
    if full_output.trim().is_empty() {
        return base.to_string();
    }

    let mut out = String::from(base);

    let signatures = extract_fatal_signatures(full_output);
    if !signatures.is_empty() {
        out.push_str("\n\n--- likely cause (scanned across the WHOLE log) ---\n");
        for sig in &signatures {
            out.push_str(&format!(
                "[{}] line {}: {}\n",
                sig.kind.label(),
                sig.line_no,
                sig.text
            ));
        }
    }

    let total_lines = full_output.lines().count();
    out.push_str(&format!(
        "\n--- {} ({} bytes, {} lines; first {}KB + last {}KB) ---\n{}",
        stream_label,
        full_output.len(),
        total_lines,
        FAILURE_EXCERPT_HEAD_BYTES / 1024,
        FAILURE_EXCERPT_TAIL_BYTES / 1024,
        head_and_tail(
            full_output,
            FAILURE_EXCERPT_HEAD_BYTES,
            FAILURE_EXCERPT_TAIL_BYTES
        )
    ));

    out
}

/// Render a failure detail that fits inside `budget` bytes, spending the
/// budget on the parts most likely to name the cause.
///
/// Used for `BuildSlot::last_build_stderr_capture` → `SlotHistory::
/// last_error_detail`, which is capped at 4 KB downstream **by dropping the
/// oldest bytes** — i.e. it too kept only a tail, and for the runner's bin
/// crate a 4 KB tail is still nothing but linker flags. Storing a rendered
/// detail instead of a raw tail means this surface answers the question as
/// well: signatures first (they are small and they are the answer), then
/// whatever head+tail context the remaining budget affords.
///
/// The result is **guaranteed** `<= budget`, enforced by a final trim from the
/// END. That guarantee is load-bearing, not cosmetic: the consumer
/// (`state::truncate_error_detail_keep_tail`) caps at exactly this size **by
/// dropping bytes from the FRONT**, so a result even slightly over budget
/// would have its `[RESOURCE EXHAUSTION] line 18: …` header sliced off — the
/// one thing this function exists to put there. Overshooting by the ~84-byte
/// elision marker was enough to do it.
pub fn render_capped_detail(full_stderr: &str, budget: usize) -> String {
    if full_stderr.len() <= budget {
        return full_stderr.to_string();
    }

    let mut out = String::new();
    let signatures = extract_fatal_signatures(full_stderr);
    if !signatures.is_empty() {
        out.push_str("--- likely cause (scanned across the WHOLE log) ---\n");
        for sig in &signatures {
            out.push_str(&format!(
                "[{}] line {}: {}\n",
                sig.kind.label(),
                sig.line_no,
                sig.text
            ));
            // Never let the signature block itself eat the whole budget.
            if out.len() > budget / 2 {
                out.push_str("[…further signatures omitted]\n");
                break;
            }
        }
        out.push('\n');
    }

    // Reserve room for `head_and_tail`'s elision marker, which is appended on
    // top of the head+tail bytes we ask for.
    const ELISION_MARKER_HEADROOM: usize = 160;
    let spent = out.len().saturating_add(ELISION_MARKER_HEADROOM);
    let remaining = budget.saturating_sub(spent);
    // Split what is left 60/40 in favour of the head, for the same reason the
    // main excerpt does: causes are emitted early.
    let head = remaining * 6 / 10;
    let tail = remaining.saturating_sub(head);
    out.push_str(&head_and_tail(full_stderr, head, tail));

    // Hard guarantee. Trims the TAIL end, so the signature block and the head
    // context — the parts that answer the question — can never be the ones
    // dropped.
    if out.len() > budget {
        let mut cut = budget;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
    }
    out
}

/// Render the operator-facing error for a build that ended WITHOUT an exit
/// status — killed by the supervisor's timeout, cancelled, or never spawned.
///
/// Identical shape to [`render_build_failure`], but it names the output as
/// PARTIAL. That distinction matters: a partial log legitimately has no
/// terminal "could not compile" summary, and a reader who assumes it is
/// complete will conclude the build was progressing fine right up to the kill
/// — when in fact the cause (a thrashing rustc, a wedged linker) is usually
/// sitting in exactly these bytes.
pub fn render_incomplete_build(base: &str, partial_stderr: &str) -> String {
    if partial_stderr.trim().is_empty() {
        return format!(
            "{base}\n\n--- no cargo output was captured before the process was killed ---"
        );
    }
    // Labelled at the source rather than string-replaced afterwards, so the
    // label and the renderer cannot drift apart.
    render_output_failure(
        base,
        partial_stderr,
        "PARTIAL cargo stderr, captured before the kill",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reconstruction of the 2026-07-31 runner build failure: the cause is on
    /// line 18, and everything after it is a multi-kilobyte rustc command line
    /// followed by the misleading exit code.
    fn oom_build_log() -> String {
        let mut log = String::new();
        for i in 1..=17 {
            log.push_str(&format!("   Compiling crate-number-{i} v0.1.0\n"));
        }
        log.push_str("memory allocation of 2097152 bytes failed\n");
        log.push_str("error: could not compile `qontinui-runner` (bin \"qontinui-runner\")\n");
        log.push_str("\nCaused by:\n  process didn't exit successfully: `rustc --crate-name qontinui_runner ");
        // The command line alone must exceed the old 2 KB tail window.
        for i in 0..200 {
            log.push_str(&format!(
                "--extern dep{i}=/very/long/path/to/libdep{i}.rlib "
            ));
        }
        log.push_str("` (exit code: 0xc0000409, STATUS_STACK_BUFFER_OVERRUN)\n");
        log
    }

    /// THE regression. The old behaviour kept only the last 2 KB, which for
    /// this log is pure linker flags — the cause was cropped entirely.
    #[test]
    fn oom_cause_survives_even_though_it_is_far_outside_the_old_tail_window() {
        let log = oom_build_log();
        assert!(
            log.len() > 4096,
            "fixture must be long enough that a 2KB tail crops the cause"
        );

        let rendered = render_build_failure("Build failed with exit code: 101", &log);

        assert!(
            rendered.contains("memory allocation of 2097152 bytes failed"),
            "the actual cause must appear in the stored error; got:\n{rendered}"
        );
        assert!(
            rendered.contains("RESOURCE EXHAUSTION"),
            "the cause must be CLASSIFIED, not just present: {rendered}"
        );
    }

    /// The cause must OUTRANK the symptom. `STATUS_STACK_BUFFER_OVERRUN` is
    /// what the machine reported; the OOM is why. Reporting them in the wrong
    /// order is what sent diagnosis down the wrong path.
    #[test]
    fn resource_exhaustion_outranks_the_abort_it_caused() {
        let sigs = extract_fatal_signatures(&oom_build_log());
        let first = sigs.first().expect("at least one signature");
        assert_eq!(
            first.kind,
            FatalKind::ResourceExhaustion,
            "the OOM must be reported before the abort it caused; got {sigs:?}"
        );
        assert!(first.text.contains("2097152"));
    }

    /// The signature must carry the line number so the reader can find it in
    /// the full sidecar even if the excerpt cropped it.
    #[test]
    fn signature_reports_its_line_number() {
        let sigs = extract_fatal_signatures(&oom_build_log());
        let oom = sigs
            .iter()
            .find(|s| s.kind == FatalKind::ResourceExhaustion)
            .expect("OOM signature");
        assert_eq!(oom.line_no, 18, "the incident's cause was on line 18");
    }

    /// A compiler diagnostic in the middle of a long log must still surface —
    /// this is the ordinary case the head+tail window alone might miss.
    #[test]
    fn compiler_diagnostic_in_the_middle_is_hoisted() {
        let mut log = "noise\n".repeat(4000);
        log.push_str("error[E0308]: mismatched types\n");
        log.push_str(&"more noise\n".repeat(4000));

        let rendered = render_build_failure("Build failed", &log);
        assert!(
            rendered.contains("error[E0308]"),
            "a mid-log diagnostic must be hoisted out of the elided region"
        );
        assert!(rendered.contains("[COMPILER]"));
    }

    /// Windows commit-limit exhaustion wears a different message than a plain
    /// OOM but is the same condition — and this box hits it constantly.
    #[test]
    fn windows_paging_file_exhaustion_is_recognized() {
        let sigs = extract_fatal_signatures(
            "error: The paging file is too small for this operation to complete. (os error 1455)",
        );
        assert_eq!(
            sigs.first().map(|s| s.kind),
            Some(FatalKind::ResourceExhaustion)
        );
    }

    #[test]
    fn disk_full_is_recognized_as_resource_exhaustion() {
        let sigs = extract_fatal_signatures("error: No space left on device (os error 28)");
        assert_eq!(
            sigs.first().map(|s| s.kind),
            Some(FatalKind::ResourceExhaustion)
        );
    }

    #[test]
    fn llvm_and_stack_overflow_aborts_are_recognized() {
        for line in [
            "LLVM ERROR: out of memory",
            "\nthread 'rustc' has overflowed its stack",
            "fatal runtime error: stack overflow",
            "thread 'main' panicked at src/lib.rs:1:1",
        ] {
            let sigs = extract_fatal_signatures(line);
            assert!(
                !sigs.is_empty(),
                "must recognize a toolchain abort in: {line:?}"
            );
        }
    }

    /// A line must be classified ONCE, by its most causal pattern — an OOM
    /// line prefixed `error:` must not also be listed as a generic error.
    #[test]
    fn a_line_is_classified_by_its_most_causal_pattern_only() {
        let sigs = extract_fatal_signatures("error: memory allocation of 64 bytes failed");
        assert_eq!(sigs.len(), 1, "one line must yield one signature: {sigs:?}");
        assert_eq!(sigs[0].kind, FatalKind::ResourceExhaustion);
    }

    #[test]
    fn clean_output_yields_no_signatures() {
        let sigs =
            extract_fatal_signatures("   Compiling foo v0.1.0\n    Finished dev target(s)\n");
        assert!(sigs.is_empty(), "got {sigs:?}");
    }

    #[test]
    fn signature_list_is_capped() {
        let log = "error[E0308]: mismatched types number ".to_string()
            + &(0..100)
                .map(|i| format!("\nerror[E0308]: mismatched types number {i}"))
                .collect::<String>();
        assert!(extract_fatal_signatures(&log).len() <= MAX_SIGNATURES);
    }

    #[test]
    fn head_and_tail_keeps_both_ends_and_marks_the_gap() {
        let s = format!("HEAD_MARKER\n{}\nTAIL_MARKER", "x\n".repeat(20_000));
        let out = head_and_tail(&s, 64, 64);
        assert!(
            out.starts_with("HEAD_MARKER"),
            "head must survive: {out:.80}"
        );
        assert!(out.ends_with("TAIL_MARKER"), "tail must survive");
        assert!(
            out.contains("bytes / ~") && out.contains("elided"),
            "the gap must be explicitly marked so adjacency is not implied"
        );
    }

    #[test]
    fn head_and_tail_returns_short_input_unchanged() {
        assert_eq!(head_and_tail("short", 1024, 1024), "short");
    }

    /// Multi-byte characters at the crop boundaries must not panic or produce
    /// invalid UTF-8. Cargo prints plenty of non-ASCII (`→`, box drawing).
    #[test]
    fn head_and_tail_is_utf8_safe_at_boundaries() {
        let s = "→".repeat(4000);
        for head in 1..8 {
            for tail in 1..8 {
                let out = head_and_tail(&s, head, tail);
                assert!(out.chars().count() > 0);
            }
        }
    }

    #[test]
    fn empty_stderr_leaves_the_base_message_alone() {
        assert_eq!(
            render_build_failure("Build failed", "   \n  "),
            "Build failed"
        );
    }

    /// A timeout-killed build must still name its cause, and must SAY the log
    /// is partial so a missing "could not compile" summary is not read as
    /// "the build was fine until we killed it".
    #[test]
    fn incomplete_build_is_labelled_partial_and_still_names_the_cause() {
        let out = render_incomplete_build(
            "Build timed out after 5400s",
            "   Compiling foo v0.1.0\nmemory allocation of 2097152 bytes failed\n",
        );
        assert!(out.contains("Build timed out after 5400s"));
        assert!(
            out.contains("PARTIAL cargo stderr"),
            "the excerpt must be labelled partial: {out}"
        );
        assert!(
            out.contains("memory allocation of 2097152 bytes failed"),
            "a timeout's cause must survive: {out}"
        );
        assert!(out.contains("RESOURCE EXHAUSTION"));
    }

    /// "We captured nothing" must be stated, not left as a bare headline that
    /// looks identical to "we did not bother looking".
    /// The 4 KB `last_error_detail` surface has the SAME head-vs-tail defect
    /// as the 2 KB one: it is truncated keeping the tail. A rendered detail
    /// must carry the cause into it, within budget.
    #[test]
    fn capped_detail_carries_the_cause_and_respects_its_budget() {
        let log = oom_build_log();
        let budget = 4 * 1024;
        let detail = render_capped_detail(&log, budget);

        assert!(
            detail.contains("memory allocation of 2097152 bytes failed"),
            "the cause must survive the 4KB detail budget: {detail}"
        );
        assert!(
            detail.len() <= budget,
            "detail must stay STRICTLY within budget (got {} for budget {budget}) — \
             the downstream cap drops bytes from the FRONT, so a single byte over \
             slices the [RESOURCE EXHAUSTION] header off",
            detail.len()
        );
    }

    /// Models the real downstream consumer
    /// (`state::truncate_error_detail_keep_tail`, which crops from the FRONT)
    /// and requires the hoisted cause to survive it. The budget assertion
    /// above passes vacuously without this.
    #[test]
    fn capped_detail_survives_a_front_cropping_consumer() {
        fn crop_front_like_state(s: String, cap: usize) -> String {
            if s.len() <= cap {
                return s;
            }
            let mut cut = s.len() - cap;
            while cut < s.len() && !s.is_char_boundary(cut) {
                cut += 1;
            }
            format!("[...truncated]\n{}", &s[cut..])
        }

        let budget = 4 * 1024;
        let detail = render_capped_detail(&oom_build_log(), budget);
        let downstream = crop_front_like_state(detail, budget);

        assert!(
            downstream.contains("[RESOURCE EXHAUSTION]"),
            "the classification header must survive the downstream front-crop: {downstream:.400}"
        );
        assert!(
            downstream.contains("memory allocation of 2097152 bytes failed"),
            "and so must the cause itself"
        );
    }

    /// Repeated identical lines must not each burn a signature slot — the
    /// dedup has to be real, not merely adjacent-collapsing.
    #[test]
    fn duplicate_signature_lines_are_really_deduplicated() {
        let log = "error: boom\nerror: other\nerror: boom\nerror: other\nerror: boom\n";
        let sigs = extract_fatal_signatures(log);
        let booms = sigs.iter().filter(|s| s.text == "error: boom").count();
        assert_eq!(booms, 1, "non-adjacent duplicates must collapse: {sigs:?}");
    }

    /// A log that already fits is passed through verbatim — no rendering
    /// overhead, no lost bytes.
    #[test]
    fn capped_detail_passes_short_logs_through_unchanged() {
        assert_eq!(render_capped_detail("short log", 4096), "short log");
    }

    /// The signature block must never consume the whole budget and leave no
    /// context behind it.
    #[test]
    fn capped_detail_bounds_the_signature_block() {
        let log = (0..500)
            .map(|i| format!("error[E{:04}]: distinct diagnostic number {i}\n", i))
            .collect::<String>();
        let budget = 2048;
        let detail = render_capped_detail(&log, budget);
        assert!(detail.len() <= budget + 256, "got {}", detail.len());
        assert!(
            detail.contains("cargo stderr")
                || detail.contains("elided")
                || detail.contains("error[E"),
            "context must still follow the signatures: {detail}"
        );
    }

    #[test]
    fn incomplete_build_with_no_output_says_so() {
        let out = render_incomplete_build("Build cancelled", "");
        assert!(out.contains("no cargo output was captured"), "got: {out}");
    }

    /// The rendered error must state the FULL size, so a reader can tell how
    /// much they are not seeing and go to the sidecar.
    #[test]
    fn rendered_error_states_the_full_log_size() {
        let log = "x\n".repeat(50_000);
        let rendered = render_build_failure("Build failed", &log);
        assert!(
            rendered.contains(&format!("{} bytes", log.len())),
            "the operator must be told the true size of the log"
        );
        assert!(
            rendered.contains("last-build.stderr"),
            "and where to find it"
        );
    }
}
