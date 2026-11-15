use super::*;

// --- syntax highlighting: runs after a patch was served, never for `plain` files ---

/// Called under the state lock right after a `Patch` event: sends the remembered colours of
/// a recently highlighted file at once, otherwise queues a highlight job for the screen
/// (or tells the running one to send its colours again).
pub(super) fn request_highlight(
    s: &Shared,
    st: &mut State,
    gen: u64,
    path: &str,
    rows: &Arc<Vec<Row>>,
    plain: bool,
) {
    if plain {
        return;
    }
    if let Some(spans) = st.hl.request(path, rows) {
        s.send(gen, highlighted(path, 0..rows.len(), rows.len(), spans));
    }
}

pub(super) fn highlighted(
    path: &str,
    range: Range<usize>,
    rows: usize,
    spans: Arc<Spans>,
) -> ReviewEvent {
    ReviewEvent::Highlighted {
        path: path.to_string(),
        rows,
        range,
        spans,
    }
}

/// Colours one file hunk by hunk from the rows on screen, handing out each step while the
/// file is on screen. Abandoned (between two rows) when the PR is closed or reopened, and
/// (between two steps) when the file is no longer on screen or near it.
pub(super) fn highlight_job(s: &Shared, gen: u64, gen_now: &AtomicU64, job: HlJob) {
    let n = job.rows.len();
    // a panic while highlighting leaves that file plain and the worker alive
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        #[cfg(test)]
        {
            let hook = lock_ok(&s.hl_hook).clone();
            if let Some(h) = hook {
                h(&job.path);
            }
        }
        let cancelled = || gen_now.load(Ordering::SeqCst) != gen;
        let on_screen = || s.visible_row(&job.path);
        let mut emit = |range: Range<usize>, spans: &[RowSpans]| {
            let st = s.lock();
            if st.gen != gen || !st.near_focus(&job.path) {
                return Next::Stop;
            }
            if st.focus.as_deref() != Some(job.path.as_str()) {
                // a prefetched file: its colours are remembered, sent once it is shown
                return Next::Continue;
            }
            s.send(
                gen,
                highlighted(&job.path, range, n, Arc::new(spans.to_vec())),
            );
            drop(st);
            s.ctx.request_repaint();
            if job.resend.swap(false, Ordering::SeqCst) {
                Next::Resend
            } else {
                Next::Continue
            }
        };
        let budget = Duration::from_millis(s.hl_budget_ms.load(Ordering::Relaxed));
        highlight_chunked(
            &job.path,
            &job.rows,
            Chunking {
                rows: CHUNK_ROWS,
                budget,
                cancelled: &cancelled,
                on_screen: &on_screen,
                emit: &mut emit,
            },
        )
    }));
    s.hl_ended.fetch_add(1, Ordering::SeqCst);
    #[cfg(test)]
    lock_ok(&s.hl_done).push((job.path.clone(), Instant::now()));
    let mut st = s.lock();
    if st.gen != gen {
        return;
    }
    let spans = match result {
        Ok((spans, HlEnd::Done | HlEnd::OverBudget)) => Some(Arc::new(spans)),
        Ok((_, HlEnd::Cancelled)) | Err(_) => None,
    };
    let resend = st.hl.finish(&job.path, spans.clone());
    let on_screen = st.focus.as_deref() == Some(job.path.as_str());
    if let (true, true, Some(spans)) = (resend, on_screen, spans) {
        // shown again after its last step: everything at once
        s.send(gen, highlighted(&job.path, 0..n, n, spans));
        drop(st);
        s.ctx.request_repaint();
    }
}

// --- end syntax highlighting ---
