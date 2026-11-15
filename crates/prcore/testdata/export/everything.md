# PR #65: Retry uploads on 5xx

- Repository: `acme/api`
- Branches: `feature/retry` -> `main`
- Head: `0123456`
- Scope: Everything
- Items: 12

## 1. PR-level [question]

**Ana Souza** · unresolved

> Is the retry count configurable?

Replies:

- **Bo Chen**
  > Not yet.

## 2. PR-level [praise]

**Me Myself** · draft

> Looks good overall.

## 3. `Cargo.lock:3`

**Renovate Bot (bot)** · unresolved · file not in diff

> Lock file regenerated.

## 4. `docs/USAGE.md:2` [suggestion]

**Bo Chen** · unresolved

> Use a longer fence:
>
> ```sh
> nit --demo
> ```
> and `code` ``ticks``.

`````diff
+````sh
+nit --demo
`````

## 5. `src/upload.rs` (file)

**Bo Chen** · unresolved

> Please add a module doc.

## 6. `src/upload.rs:9` [praise]

**Bo Chen** · resolved

> Nice cleanup.

```diff
 fn upload(req: &Req) {
```

Replies:

- **Me Myself** · draft
  > Agreed, resolved.

## 7. `src/upload.rs:old 10`

**Me Myself** · draft · outdated

> Written on an older push.

## 8. `src/upload.rs:11` [issue]

**Me Myself** · draft

> Name this constant.

```diff
     let n = 3;
-    retry(n);
+    retry(n * 2);
```

## 9. `src/upload.rs:old 11`

**Ana Souza** · unresolved

> Why was this removed?

```diff
 fn upload(req: &Req) {
     let n = 3;
-    retry(n);
```

## 10. `src/upload.rs:12` [nit]

**Zoë Ørsted** · unresolved

> Ce n'est pas très clair — 名前を変えて?
> Second line.
>
>   indented third.

```diff
-    retry(n);
+    retry(n * 2);
+    log("done ✓");
```

Replies:

- **Bo Chen** · deleted
  > [deleted]

- **Ana Souza** [nit]
  > Agreed.

- **Me Myself** · draft
  > Thanks!
  > Follow-up in #66.

## 11. `src/upload.rs:40` [issue]

**Ana Souza** · unresolved · outdated

> This leaks the handle.

## 12. Reply to comment #999

**Me Myself** · draft

> Reply to a thread that is gone.
