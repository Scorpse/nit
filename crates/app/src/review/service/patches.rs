use super::*;

/// A built patch: rows, whether it renders plain, and its layout.
pub(super) type Built = (Arc<Vec<Row>>, bool, Arc<PatchLayout>);

pub(super) fn build_patch(s: &Shared, sess: &Session, path: &str) -> Result<Built, String> {
    #[cfg(test)]
    {
        let hook = lock_ok(&s.patch_hook).clone();
        if let Some(h) = hook {
            h(path);
        }
    }
    let Some(&i) = sess.index.get(path) else {
        return Err("This file is not part of the pull request".into());
    };
    let f = &sess.files[i];
    let git = s.git().map_err(|e| format!("Git is unavailable: {e}"))?;
    let text = file_patch(git, &sess.local, &sess.refs, f, s.patch_auth(sess))
        .map_err(|e| format!("Could not load the diff: {e}"))?;
    Ok(built_from(&text))
}

/// Rows, plain flag and layout of one file's patch text (from `file_patch` or a batch:
/// the same bytes, so the same rows).
pub(super) fn built_from(text: &str) -> Built {
    let rows = parse_patch(text);
    let plain = plain_rows(&rows);
    let layout = Arc::new(PatchLayout::compute(&rows, plain));
    (Arc::new(rows), plain, layout)
}

/// Built patches of a batch, and the paths it could not serve.
pub(super) type BatchOut = (Vec<(String, Built)>, Vec<String>);

pub(super) fn run_batch(
    s: &Shared,
    gen: u64,
    sess: &Session,
    batch: &Batch,
) -> Result<BatchOut, String> {
    #[cfg(test)]
    {
        let hook = lock_ok(&s.batch_hook).clone();
        if let Some(h) = hook {
            h(if batch.whole_pr { "whole" } else { "near" });
        }
    }
    let files: Vec<&DiffFile> = batch
        .paths
        .iter()
        .filter_map(|p| sess.index.get(p).map(|&i| &sess.files[i]))
        .collect();
    let git = s.git()?;
    // closing or reopening kills the running `git diff` (and its children) within ~50 ms
    let cancelled = || !s.current(gen);
    let out = batch_patches_cancellable(
        git,
        &sess.local,
        &sess.refs,
        &files,
        s.patch_auth(sess),
        &cancelled,
    )
    .map_err(|e| e.to_string())?;
    let built = out
        .found
        .into_iter()
        .map(|(i, text)| (files[i].path.clone(), built_from(&text)))
        .collect();
    let missing = out
        .missing
        .into_iter()
        .map(|i| files[i].path.clone())
        .collect();
    Ok((built, missing))
}

/// Runs one batch and puts its patches into the cache (never replacing a cached one). A
/// queued request for one of its files is answered from the batch. Whatever a neighbour
/// batch could not serve (or all of it, on an error or a panic) is prefetched per file.
pub(super) fn batch_job(s: &Shared, gen: u64, sess: &Session, batch: Batch) {
    let result = if batch.paths.is_empty() {
        Ok((Vec::new(), Vec::new()))
    } else {
        std::panic::catch_unwind(AssertUnwindSafe(|| run_batch(s, gen, sess, &batch)))
            .unwrap_or_else(|_| Err("internal error".into()))
    };
    s.batches_run.fetch_add(1, Ordering::Relaxed);
    let mut st = s.lock();
    if st.gen != gen {
        s.batches_dropped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if !batch.whole_pr {
        for p in &batch.paths {
            st.batching.remove(p);
        }
    }
    let (built, missing) = match result {
        Ok(r) => r,
        Err(_) => (Vec::new(), batch.paths.clone()),
    };
    let mut served = false;
    for (path, (rows, plain, layout)) in built {
        if st.cache.contains(&path) {
            continue;
        }
        st.cache.insert(path.clone(), rows.clone(), layout.clone());
        // asked for meanwhile but not started yet: answer now instead of building it again
        if let Some(pos) = st.queue.iter().position(|j| j.path == path && j.wanted) {
            st.queue.remove(pos);
            st.queued.remove(&path);
            serve(s, &mut st, gen, sess, &path, (rows, plain, layout));
            served = true;
        } else {
            st.prefetched(&path, &rows, plain);
        }
    }
    if !batch.whole_pr {
        for p in &missing {
            st.prefetch_one(p);
        }
    }
    drop(st);
    s.work.notify_all();
    if served {
        s.ctx.request_repaint();
    }
}

/// Under the state lock: sends a built patch to the screen, asks for its colours (for the
/// screen if it is the file on screen, else in the background) and prefetches its
/// neighbours.
pub(super) fn serve(
    s: &Shared,
    st: &mut State,
    gen: u64,
    sess: &Session,
    path: &str,
    built: Built,
) {
    let (rows, plain, layout) = built;
    if st.focus.is_none() {
        st.focus = Some(path.to_string());
    }
    s.send(
        gen,
        ReviewEvent::Patch {
            path: path.to_string(),
            rows: rows.clone(),
            plain,
            layout,
        },
    );
    if st.focus.as_deref() == Some(path) {
        request_highlight(s, st, gen, path, &rows, plain);
    } else {
        // asked for, then passed (j/k held): not on screen any more
        st.prefetched(path, &rows, plain);
    }
    st.prefetch_around(sess, path);
}
