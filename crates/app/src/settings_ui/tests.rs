use super::*;

#[test]
fn every_forge_can_be_added() {
    let enabled: Vec<Forge> = forge_choices()
        .into_iter()
        .filter(|(_, on)| *on)
        .map(|(f, _)| f)
        .collect();
    assert_eq!(
        enabled,
        vec![Forge::BitbucketCloud, Forge::GitHub, Forge::GitLab]
    );
}

#[test]
fn gitlab_form_derives_the_bases_for_self_managed_and_needs_a_token() {
    let mut f = AddAccountForm {
        forge: Forge::GitLab,
        token: " glpat-tok \n".into(),
        ..AddAccountForm::default()
    };
    // empty address: gitlab.com
    match f.submit().unwrap() {
        NewAccount::GitLab { bases, token } => {
            assert_eq!(bases.api_base, "https://gitlab.com/api/v4");
            assert_eq!(bases.git_host, "gitlab.com");
            assert_eq!(token, "glpat-tok");
        }
        other => panic!("{other:?}"),
    }
    // self-managed under a relative URL root
    f.web_url = "https://git.corp.example/gitlab/".into();
    match f.submit().unwrap() {
        NewAccount::GitLab { bases, .. } => {
            assert_eq!(bases.api_base, "https://git.corp.example/gitlab/api/v4");
            assert_eq!(bases.git_host, "git.corp.example/gitlab");
        }
        other => panic!("{other:?}"),
    }
    f.web_url = "http://git.corp.example".into();
    assert!(f.submit().unwrap_err().contains("https"));
    f.web_url.clear();
    f.token.clear();
    assert!(f.submit().unwrap_err().contains("token"));
    f.token = "glpat-s3cret".into();
    assert!(!format!("{:?}", f.submit().unwrap()).contains("glpat-s3cret"));
}

#[test]
fn an_http_address_needs_a_private_host_and_the_cleartext_tick() {
    for forge in [Forge::GitLab, Forge::GitHub] {
        let mut f = AddAccountForm {
            forge,
            token: "tok".into(),
            web_url: " http://192.168.1.20:8080/ ".into(),
            ..AddAccountForm::default()
        };
        assert!(f.cleartext(), "{forge:?}");
        assert!(!f.can_submit());
        let e = f.submit().unwrap_err();
        assert!(e.contains("I understand"), "{e}");
        f.cleartext_ok = true;
        assert!(f.can_submit());
        match f.submit().unwrap() {
            NewAccount::GitLab { bases, .. } => {
                assert_eq!(bases.api_base, "http://192.168.1.20:8080/api/v4")
            }
            NewAccount::GitHub { bases, .. } => {
                assert_eq!(bases.api_base, "http://192.168.1.20:8080/api/v3")
            }
            other => panic!("{other:?}"),
        }
        // ticking never opens http to a public or disguised host
        for bad in [
            "http://gitlab.example",
            "http://8.8.8.8",
            "http://192.168.1.1.evil.com",
            "http://192.168.1.1@evil.com",
        ] {
            f.web_url = bad.into();
            assert!(f.submit().unwrap_err().contains("https"), "{bad}");
        }
        f.web_url = "https://git.corp.example".into();
        f.cleartext_ok = false;
        assert!(!f.cleartext());
        assert!(f.can_submit());
        assert!(f.submit().is_ok());
    }
    // Bitbucket stays cloud-only: its form has no address at all
    let b = AddAccountForm {
        web_url: "http://192.168.1.20:8080".into(),
        ..AddAccountForm::default()
    };
    assert!(!b.cleartext());
    assert_eq!(CLEARTEXT_WARNING, prcore::origin::CLEARTEXT_WARNING);
}

#[test]
fn an_http_account_is_flagged_in_the_list() {
    let mut accounts = crate::demo::demo_accounts();
    let others = other_accounts(&accounts);
    assert!(others.iter().all(|o| !o.cleartext));
    let last = accounts.last_mut().unwrap();
    last.web_base = "http://192.168.1.20:8080".into();
    last.api_base = "http://192.168.1.20:8080/api/v4".into();
    last.git_host = "192.168.1.20:8080".into();
    let others = other_accounts(&accounts);
    let flagged = others.last().unwrap();
    assert!(flagged.cleartext);
    assert_eq!(cleartext_chip(flagged), Some(("http", Tone::Warn)));
    assert_eq!(cleartext_chip(&others[0]), None);
}

#[test]
fn expiry_warning_shows_on_the_account_row_within_14_days() {
    let today = jiff::civil::Date::new(2026, 11, 14).unwrap();
    let mut a = other_accounts(&crate::demo::demo_accounts()).remove(0);
    assert_eq!(account_expiry_status(&a, today), None);
    a.expires = jiff::civil::Date::new(2026, 11, 24).ok();
    let (text, tone) = account_expiry_status(&a, today).unwrap();
    assert!(text.contains("expires in 10 day(s)"), "{text}");
    assert_eq!(tone, Tone::Warn);
    a.expires = jiff::civil::Date::new(2027, 11, 24).ok();
    assert_eq!(account_expiry_status(&a, today), None);
}

#[test]
fn add_form_cleans_and_checks_its_fields() {
    let mut f = AddAccountForm {
        email: "  bo@example.com ".into(),
        token: " ATATT-abc \n".into(),
        ..AddAccountForm::default()
    };
    assert_eq!(
        f.submit(),
        Ok(NewAccount::Bitbucket {
            email: "bo@example.com".to_string(),
            token: "ATATT-abc".to_string()
        })
    );
    f.token.clear();
    assert_eq!(f.submit(), Err("Paste an API token".to_string()));
    f.email = "bo".into();
    assert!(f.submit().unwrap_err().contains("email"));
}

#[test]
fn github_form_derives_the_bases_and_needs_https_and_a_token() {
    let mut f = AddAccountForm {
        forge: Forge::GitHub,
        token: " gh-tok \n".into(),
        ..AddAccountForm::default()
    };
    // empty address: github.com
    match f.submit().unwrap() {
        NewAccount::GitHub { bases, token } => {
            assert_eq!(bases.api_base, "https://api.github.com");
            assert_eq!(bases.git_host, "github.com");
            assert_eq!(token, "gh-tok");
        }
        other => panic!("{other:?}"),
    }
    f.web_url = "https://ghe.corp.example/".into();
    match f.submit().unwrap() {
        NewAccount::GitHub { bases, .. } => {
            assert_eq!(bases.api_base, "https://ghe.corp.example/api/v3");
            assert_eq!(bases.graphql_url, "https://ghe.corp.example/api/graphql");
        }
        other => panic!("{other:?}"),
    }
    f.web_url = "http://ghe.corp.example".into();
    assert!(f.submit().unwrap_err().contains("https"));
    f.web_url.clear();
    f.token = "  ".into();
    assert_eq!(f.submit(), Err("Paste a personal access token".to_string()));
    // the token never shows in Debug
    f.token = "gh-s3cret".into();
    assert!(!format!("{:?}", f.submit().unwrap()).contains("gh-s3cret"));
}

#[test]
fn other_accounts_leave_out_the_signed_in_one() {
    let accounts = crate::demo::demo_accounts();
    let others = other_accounts(&accounts);
    assert_eq!(others.len(), 3);
    assert_eq!(
        (others[2].forge, others[2].host.as_str()),
        (Forge::GitLab, "git.acme.example/gitlab")
    );
    assert_eq!(others[0].name, "Bo Lindqvist");
    assert_eq!(
        (others[0].forge, others[0].host.as_str()),
        (Forge::BitbucketCloud, "bitbucket.org")
    );
    assert_eq!(
        (others[1].forge, others[1].host.as_str()),
        (Forge::GitHub, "github.com")
    );
    assert_eq!(other_account_status(&others[0]).1, Tone::Good);
    let off = OtherAccount {
        enabled: false,
        ..others[0].clone()
    };
    assert_eq!(other_account_status(&off).1, Tone::Muted);
}

/// Absolute paths on the host (a drive path is relative on macOS and Linux), for tests
/// that need the string checks to pass.
const HOST_M: &str = if cfg!(windows) { r"C:\m" } else { "/m" };
const HOST_CODE: &str = if cfg!(windows) { r"D:\code" } else { "/code" };
const HOST_CODE_CLONES: &str = if cfg!(windows) {
    r"D:\code\clones"
} else {
    "/code/clones"
};
const HOST_CLONES: &str = if cfg!(windows) {
    r"D:\clones"
} else {
    "/clones"
};

fn state_with(download: &str, clone: &str) -> SettingsState {
    let mut s = SettingsState::new(
        egui::Context::default(),
        Ok(&Settings {
            clone_root: None,
            download_dir: PathBuf::from(download),
        }),
        PathBuf::from(r"C:\default"),
    );
    s.clone.text = clone.into();
    s
}

#[test]
fn validate_download_dir_rejects_file_and_unwritable() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("a.txt");
    std::fs::write(&file, "x").unwrap();
    assert!(fs_check_download(&file).unwrap_err().contains("file"));
    // below a file: cannot be created
    assert!(fs_check_download(&file.join("sub"))
        .unwrap_err()
        .contains("file"));
    // missing folder below a writable one is fine (creatable) and is not created
    let fresh = tmp.path().join("x").join("y");
    assert_eq!(fs_check_download(&fresh), Ok(()));
    assert!(!fresh.exists());
    // an existing writable folder leaves no probe behind
    assert_eq!(fs_check_download(tmp.path()), Ok(()));
    let leftovers = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(".nit-write"))
        .count();
    assert_eq!(leftovers, 0);
    // Unwritable on every OS: a directory already sits where the probe file would go.
    let blocked = tmp.path().join("blocked");
    std::fs::create_dir(&blocked).unwrap();
    std::fs::create_dir(blocked.join("probe")).unwrap();
    let err = check_writable_with(&blocked, || "probe".into()).unwrap_err();
    assert!(err.starts_with("Not writable"), "{err}");
    assert!(blocked.join("probe").is_dir(), "the blocker is left alone");
}

#[test]
fn probe_names_are_unique() {
    assert_ne!(probe_name(), {
        std::thread::sleep(std::time::Duration::from_millis(2));
        probe_name()
    });
}

#[test]
fn validate_clone_root_optional() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(sync_check_clone("").unwrap(), None);
    assert_eq!(sync_check_clone("  ").unwrap(), None);
    assert_eq!(fs_check_clone(tmp.path()), Ok(()));
    assert!(fs_check_clone(&tmp.path().join("nope"))
        .unwrap_err()
        .contains("exist"));
    let file = tmp.path().join("f");
    std::fs::write(&file, "x").unwrap();
    assert!(fs_check_clone(&file).unwrap_err().contains("file"));
    // relative text is refused before the disk is looked at
    assert_eq!(sync_check_clone("code").unwrap_err(), FULL_PATH_HINT);
}

#[test]
fn relative_paths_are_refused_and_pasted_quotes_stripped() {
    for bad in ["mirrors", r"..\x", r"C:foo", r"\foo", "./a"] {
        assert_eq!(check_absolute(bad).unwrap_err(), FULL_PATH_HINT, "{bad}");
        assert_eq!(sync_check_download(bad).unwrap_err(), FULL_PATH_HINT);
    }
    assert_eq!(sync_check_download("  ").unwrap_err(), "Choose a folder");
    assert_eq!(clean_path_text(r#"  "C:\a b"  "#), r"C:\a b");
    assert_eq!(clean_path_text(r#""  C:\a  ""#), r"C:\a");
    assert_eq!(
        clean_path_text(r#""C:\a"#),
        r#""C:\a"#,
        "unmatched quote kept"
    );
    let tmp = tempfile::tempdir().unwrap();
    let quoted = format!("  \"{}\"  ", tmp.path().display());
    assert_eq!(check_absolute(&quoted).unwrap(), tmp.path());
    #[cfg(windows)]
    {
        assert!(check_absolute(r"D:\nit\mirrors").is_ok());
        assert!(check_absolute(r"\\server\share\x").is_ok());
    }
}

#[test]
fn nested_folders_are_rejected() {
    let nested = [
        (r"D:\code", r"D:\code"),
        (r"D:\code\mirrors", r"D:\code"),
        (r"D:\code", r"D:\code\sub\deep"),
        (r"D:\Code", r"d:\CODE"),
        (r"D:\code\", r"D:/code"),
        (r"D:/code/mirrors/", r"d:\code"),
        (r"D:\code\a\..\mirrors", r"D:\code"),
        (r"D:\code", r"D:\code\.\x"),
    ];
    for (d, c) in nested {
        let e = check_not_nested(d, c).unwrap_err();
        assert!(e.contains(d.trim()) && e.contains(c), "{d} / {c}: {e}");
    }
    let fine = [
        (r"D:\mirrors", r"D:\code"),
        (r"D:\code2", r"D:\code"),
        (r"D:\code", r"D:\code-old"),
        (r"E:\code", r"D:\code"),
        (r"D:\code\mirrors", r"D:\code\clones"),
        (r"D:\mirrors", ""),
        ("", r"D:\code"),
    ];
    for (d, c) in fine {
        assert_eq!(check_not_nested(d, c), Ok(()), "{d} / {c}");
    }
}

#[test]
fn save_persists_via_store() {
    struct Probe(std::sync::Arc<std::sync::Mutex<store::Store>>);
    impl Probe {
        fn settings(&self, d: &Path) -> store::StoreResult<Settings> {
            self.0.lock().unwrap().settings(d)
        }
    }
    let tmp = tempfile::tempdir().unwrap();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(
        store::Store::open_in_memory().unwrap(),
    ));
    let writer = StoreWriter::new(shared.clone());
    let store = Probe(shared);
    let default = tmp.path().join("default");
    let initial = store.settings(&default).unwrap();
    assert_eq!(initial.download_dir, default);
    assert_eq!(initial.clone_root, None);

    let mirrors = tmp.path().join("mirrors");
    let clones = tmp.path().join("clones");
    std::fs::create_dir(&clones).unwrap();
    let checked = run_save_checks(
        &mirrors.display().to_string(),
        &format!("\"{}\"", clones.display()),
    )
    .unwrap();
    assert!(mirrors.is_dir(), "download folder is created by the checks");

    let mut s = state_with(&mirrors.display().to_string(), "");
    s.persist(&checked, &writer);
    writer.flush();
    assert_eq!(s.poll(), None);
    assert_eq!(s.message, Some(Ok("Saved".into())));
    assert_eq!(store.settings(&default).unwrap(), checked);
    assert_eq!(checked.download_dir, mirrors);
    assert_eq!(checked.clone_root.as_deref(), Some(clones.as_path()));

    // clearing the optional folder removes it
    let cleared = run_save_checks(&mirrors.display().to_string(), "").unwrap();
    s.persist(&cleared, &writer);
    writer.flush();
    assert_eq!(store.settings(&default).unwrap().clone_root, None);

    // invalid input yields an error and saves nothing
    let file = tmp.path().join("a.txt");
    std::fs::write(&file, "x").unwrap();
    assert!(run_save_checks(&file.display().to_string(), "").is_err());
    assert!(run_save_checks("relative", "").is_err());
    assert!(run_save_checks(
        &mirrors.display().to_string(),
        &tmp.path().display().to_string()
    )
    .unwrap_err()
    .contains("inside one another"));
    assert_eq!(store.settings(&default).unwrap().download_dir, mirrors);
}

#[test]
fn load_failure_blocks_save() {
    let mut s = SettingsState::new(
        egui::Context::default(),
        Err("database error".into()),
        PathBuf::from(r"C:\default"),
    );
    assert!(s.load_error.is_some());
    assert!(!s.can_save());
    s.begin_save();
    assert!(!s.saving);
}

#[test]
fn the_path_examples_are_full_paths_of_this_platform() {
    let example = FULL_PATH_HINT.rsplit("e.g. ").next().unwrap();
    assert_eq!(example, EXAMPLE_DOWNLOAD);
    assert!(
        check_absolute(EXAMPLE_DOWNLOAD).is_ok(),
        "{EXAMPLE_DOWNLOAD}"
    );
    let clones = EXAMPLE_CLONES.trim_end_matches(" (optional)");
    assert!(check_absolute(clones).is_ok(), "{clones}");
}

#[test]
fn save_is_blocked_by_string_checks() {
    let mut s = state_with(HOST_CODE, HOST_CODE_CLONES);
    assert!(!s.can_save(), "nested");
    s.clone.text = HOST_CLONES.into();
    assert!(s.can_save());
    s.download.text = "mirrors".into();
    assert!(!s.can_save(), "relative");
}

#[test]
fn stale_scan_results_are_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = state_with(r"C:\m", &tmp.path().display().to_string());
    s.rescan();
    let first = s.scan_generation;
    assert_eq!(s.scan, Scan::Running);
    s.rescan(); // restart while running
    let second = s.scan_generation;
    assert!(second > first);
    // the older result is ignored, the newer one applies
    s.apply(Worker::Scanned {
        generation: first,
        result: Ok((vec![], vec![])),
    });
    assert_eq!(s.scan, Scan::Running);
    s.apply(Worker::Scanned {
        generation: second,
        result: Err("boom".into()),
    });
    assert_eq!(s.scan, Scan::Failed("boom".into()));
    // editing the clone folder invalidates the in-flight scan
    s.rescan();
    let g = s.scan_generation;
    s.edited(Field::CloneRoot);
    s.apply(Worker::Scanned {
        generation: g,
        result: Ok((vec![], vec![])),
    });
    assert_eq!(s.scan, Scan::Idle);
    // Reset bumps the generation too
    s.rescan();
    let g = s.scan_generation;
    s.reset_to_default();
    assert!(s.scan_generation > g);
    s.apply(Worker::Scanned {
        generation: g,
        result: Ok((vec![], vec![])),
    });
    assert_eq!(s.scan, Scan::Idle);
}

#[test]
fn stale_check_and_save_results_are_dropped() {
    let mut s = state_with(HOST_M, "");
    s.request_check(Field::DownloadDir);
    let g = s.download.generation;
    assert_eq!(s.download.check, Check::Checking);
    s.edited(Field::DownloadDir);
    s.apply(Worker::Checked {
        field: Field::DownloadDir,
        generation: g,
        result: Err("old".into()),
    });
    assert_eq!(s.download.check, Check::Unchecked);
    s.apply(Worker::Checked {
        field: Field::DownloadDir,
        generation: s.download.generation,
        result: Err("new".into()),
    });
    assert_eq!(s.download.check, Check::Failed("new".into()));

    // a Save result from before a Reset is not persisted
    s.saving = true;
    s.save_generation = 5;
    let stale = Settings {
        clone_root: None,
        download_dir: PathBuf::from(r"C:\m"),
    };
    s.save_generation += 1;
    assert!(s
        .apply(Worker::Saved {
            generation: 5,
            result: Ok(stale.clone())
        })
        .is_none());
    assert!(s
        .apply(Worker::Saved {
            generation: 6,
            result: Ok(stale.clone())
        })
        .is_some());
}

#[test]
fn browse_is_coalesced_until_the_dialog_returns() {
    let mut s = state_with(r"C:\m", "");
    s.browsing = true;
    s.browse(Field::CloneRoot); // ignored: no second dialog
    assert!(s.browsing);
    s.apply(Worker::Picked(Field::CloneRoot, None)); // cancelled
    assert!(!s.browsing);
    assert_eq!(s.clone.text, "");
    s.browsing = true;
    s.apply(Worker::Picked(
        Field::DownloadDir,
        Some(PathBuf::from(r"D:\x")),
    ));
    assert!(!s.browsing);
    assert_eq!(s.download.text, r"D:\x");
}

#[test]
fn reset_restores_default_download_and_clears_clone_root() {
    let mut s = state_with(r"D:\other", r"D:\code");
    s.reset_to_default();
    assert_eq!(s.download.text, r"C:\default");
    assert!(s.clone.text.is_empty());
}

#[test]
fn save_needs_a_change_and_a_save_moves_the_baseline() {
    let tmp = tempfile::tempdir().unwrap();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(
        store::Store::open_in_memory().unwrap(),
    ));
    let writer = StoreWriter::new(shared);
    let mut s = state_with(r"C:\m", "");
    assert_eq!(s.save_blocked(), Some("Nothing changed"));
    s.download.text = r" c:/M\ ".into();
    assert!(!s.changed(), "the same folder spelled differently");
    let mirrors = tmp.path().join("mirrors");
    s.download.text = mirrors.display().to_string();
    assert!(s.changed() && s.can_save());
    s.clone.text = "code".into();
    assert_eq!(s.save_blocked(), Some("Fix the folder above to save"));
    s.clone.text.clear();

    let checked = run_save_checks(&s.download.text, "").unwrap();
    s.persist(&checked, &writer);
    writer.flush();
    s.poll();
    assert_eq!(s.message, Some(Ok("Saved".into())));
    assert!(!s.changed(), "saved values are the new baseline");
    assert_eq!(s.save_blocked(), Some("Nothing changed"));
}

#[test]
fn a_failed_store_write_keeps_the_old_baseline() {
    let mut s = state_with(r"C:\m", "");
    s.download.text = r"C:\n".into();
    s.pending = Some(Settings {
        clone_root: None,
        download_dir: PathBuf::from(r"C:\n"),
    });
    s.apply(Worker::Persisted(Err("disk full".into())));
    assert!(s.changed());
    assert!(matches!(&s.message, Some(Err(e)) if e.contains("disk full")));
}

#[test]
fn reset_is_only_offered_away_from_the_defaults() {
    let mut s = state_with(r"D:\other", r"D:\code");
    assert!(!s.at_default());
    s.reset_to_default();
    assert!(s.at_default());
    assert!(s.changed(), "Reset fills the fields; Save keeps them");
}

#[test]
fn open_errors_stay_with_their_card() {
    let mut s = state_with(r"C:\m", "");
    s.apply(Worker::Opened(Err(("data", "no".into()))));
    assert_eq!(s.open_error, Some(("data", "no".into())));
    s.apply(Worker::Opened(Ok(())));
    assert_eq!(s.open_error, None);
    let tmp = tempfile::tempdir().unwrap();
    assert!(open_in_explorer(&tmp.path().join("missing"))
        .unwrap_err()
        .contains("does not exist"));
    assert!(open_in_explorer(Path::new("relative")).is_err());
}

#[test]
fn checked_results_carry_the_folder_info() {
    let mut s = state_with(r"C:\m", "");
    s.request_check(Field::DownloadDir);
    let info = FolderInfo {
        exists: false,
        free: Some(1),
    };
    s.apply(Worker::Checked {
        field: Field::DownloadDir,
        generation: s.download.generation,
        result: Ok(info),
    });
    assert_eq!(s.download.check, Check::Ok(info));
}

#[test]
fn pasted_paths_lose_spaces_and_either_quote_pair() {
    assert_eq!(clean_path_text(" 'D:\\code' \t"), r"D:\code");
    assert_eq!(clean_path_text("\"D:\\code\"\n"), r"D:\code");
    assert_eq!(clean_path_text("D:\\code   "), r"D:\code");
    assert_eq!(
        clean_path_text("'D:\\code\""),
        "'D:\\code\"",
        "mixed quotes kept"
    );
    assert_eq!(check_absolute(" 'relative' ").unwrap_err(), FULL_PATH_HINT);
}

#[test]
fn storage_changed_ignores_spelling_of_the_same_folder() {
    let saved = (r"D:\nit\mirrors", r"D:\code");
    assert!(!storage_changed(saved, r"D:\nit\mirrors", r"D:\code"));
    assert!(!storage_changed(
        saved,
        r" d:/NIT/mirrors\ ",
        "\"D:\\code\""
    ));
    assert!(storage_changed(saved, r"D:\nit\mirrors2", r"D:\code"));
    assert!(storage_changed(saved, r"D:\nit\mirrors", ""));
    assert!(!storage_changed((r"C:\m", ""), r"C:\m", "  "));
}

#[test]
fn git_versions_are_parsed_and_old_or_missing_git_warns() {
    assert_eq!(
        parse_git_version("git version 2.47.1.windows.1\n"),
        Some((2, 47, 1))
    );
    assert_eq!(parse_git_version("git version 2.31"), Some((2, 31, 0)));
    assert_eq!(parse_git_version("git version x.y"), None);
    assert_eq!(parse_git_version("hello"), None);

    let ok = Ok("git version 2.47.1.windows.1".to_string());
    assert_eq!(git_info(Some(&ok)), GitInfo::Ok("2.47.1".into()));
    let edge = Ok("git version 2.31.0".to_string());
    assert_eq!(git_info(Some(&edge)), GitInfo::Ok("2.31.0".into()));
    let old = Ok("git version 2.30.9".to_string());
    assert_eq!(git_info(Some(&old)), GitInfo::Old("2.30.9".into()));
    let old_major = Ok("git version 1.99.0".to_string());
    assert_eq!(git_info(Some(&old_major)), GitInfo::Old("1.99.0".into()));
    let odd = Ok("git version custom".to_string());
    assert_eq!(
        git_info(Some(&odd)),
        GitInfo::Ok("git version custom".into())
    );
    assert_eq!(git_info(Some(&Err("x".into()))), GitInfo::Missing);
    assert_eq!(git_info(None), GitInfo::Checking);

    let (v, chip, hint) = git_row(&GitInfo::Ok("2.47.1".into()));
    assert_eq!(
        (v.as_str(), chip, hint),
        ("2.47.1", Some(("OK", Tone::Good)), None)
    );
    let (_, chip, hint) = git_row(&GitInfo::Old("2.30.0".into()));
    assert_eq!(chip.map(|c| c.1), Some(Tone::Warn));
    assert!(hint.unwrap().contains("2.31"));
    let (v, chip, hint) = git_row(&GitInfo::Missing);
    assert_eq!(
        (v.as_str(), chip.map(|c| c.1)),
        ("not found", Some(Tone::Bad))
    );
    assert!(hint.is_some());
    assert_eq!(git_row(&GitInfo::Checking).1, None);
}

#[test]
fn repository_summary_and_fraction() {
    assert_eq!(repos_summary(0, 0).0, "No repositories listed yet");
    assert_eq!(repos_summary(0, 12).0, "None of 12 chosen");
    assert!(repos_summary(0, 12).1.contains("Nothing is synced"));
    assert_eq!(repos_summary(3, 12).0, "3 of 12 chosen");
    assert_eq!(repos_fraction(0, 0), 0.0);
    assert_eq!(repos_fraction(3, 12), 0.25);
    assert_eq!(repos_fraction(20, 12), 1.0, "clamped");
}

#[test]
fn adapter_lines_are_split_into_name_and_backend() {
    assert_eq!(
        adapter_parts("NVIDIA GeForce RTX 4070 (Dx12)"),
        Some(("NVIDIA GeForce RTX 4070".into(), "DirectX 12".into()))
    );
    assert_eq!(
        adapter_parts("Intel(R) UHD Graphics (Vulkan)"),
        Some(("Intel(R) UHD Graphics".into(), "Vulkan".into()))
    );
    assert_eq!(
        adapter_parts("llvmpipe (LLVM 15.0.7, 256 bits) (Gl)"),
        Some(("llvmpipe (LLVM 15.0.7, 256 bits)".into(), "OpenGL".into()))
    );
    assert_eq!(adapter_parts("X (Noop)").unwrap().1, "Noop");
    assert_eq!(adapter_parts("not started"), None);
    assert_eq!(adapter_parts(" (Dx12)"), None);
}

#[test]
fn bytes_and_download_lines() {
    const GB: u64 = 1024 * 1024 * 1024;
    assert_eq!(format_bytes(0), "0 bytes");
    assert_eq!(format_bytes(1023), "1023 bytes");
    assert_eq!(format_bytes(1536), "2 KB");
    assert_eq!(format_bytes(300 * 1024 * 1024), "300 MB");
    assert_eq!(format_bytes(GB * 11 / 2), "5.5 GB");
    assert_eq!(format_bytes(2048 * GB), "2.0 TB");

    let (t, tone) = download_line(FolderInfo {
        exists: true,
        free: Some(120 * GB),
    });
    assert_eq!(t, "Exists and is writable \u{00b7} 120.0 GB free");
    assert_eq!(tone, Tone::Good);
    let (t, tone) = download_line(FolderInfo {
        exists: false,
        free: None,
    });
    assert_eq!(t, "Does not exist yet; Save creates it");
    assert_eq!(tone, Tone::Good);
    let (t, tone) = download_line(FolderInfo {
        exists: true,
        free: Some(2 * GB),
    });
    assert!(t.contains("only 2.0 GB free"), "{t}");
    assert_eq!(tone, Tone::Warn);
}

#[test]
fn free_space_and_probe_of_a_real_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let info = fs_probe_download(tmp.path()).unwrap();
    assert!(info.exists);
    let missing = fs_probe_download(&tmp.path().join("new")).unwrap();
    assert!(!missing.exists, "not created by the probe");
    assert!(!tmp.path().join("new").exists());
    #[cfg(any(windows, unix))]
    assert!(free_space(&tmp.path().join("a").join("b")).is_some_and(|f| f > 0));
}

#[test]
fn clone_lines_and_initials() {
    assert_eq!(clones_line(0, 0), "No usable clones found");
    assert_eq!(clones_line(1, 0), "1 usable clone found");
    assert_eq!(
        clones_line(12, 2),
        "12 usable clones found \u{00b7} 2 skipped"
    );
    assert_eq!(initials("Ana Lopez"), "AL");
    assert_eq!(initials("  ana  maria   lopez "), "AL");
    assert_eq!(initials("bob"), "B");
    assert_eq!(initials(""), "?");
    assert_eq!(initials("élodie roux"), "ÉR");
}

#[test]
fn connection_state_follows_the_sync_status() {
    let c = |demo, conn, run, note: Option<&str>, me| connection(demo, conn, run, (2, 5), note, me);
    assert_eq!(c(true, true, true, Some("x"), true), Conn::Demo);
    assert_eq!(c(false, true, true, None, true), Conn::Connecting);
    assert_eq!(
        c(false, false, true, Some("x"), true),
        Conn::Syncing { done: 2, total: 5 }
    );
    assert_eq!(
        c(false, false, false, Some("Sync failed"), true),
        Conn::Problem("Sync failed".into())
    );
    assert_eq!(c(false, false, false, None, true), Conn::Connected);
    assert_eq!(c(false, false, false, None, false), Conn::Offline);
    assert_eq!(conn_label(&Conn::Connected).1, Tone::Good);
    assert!(conn_label(&Conn::Syncing { done: 2, total: 5 })
        .2
        .contains("2 of 5"));
    assert_eq!(conn_label(&Conn::Problem("boom".into())).2, "boom");
}

#[test]
fn review_button_state_table() {
    use PrState::*;
    use ReviewButton::*;
    let st = review_button_state;
    assert_eq!(st(true, Open, true, true), Hidden);
    assert_eq!(st(true, Merged, false, false), Hidden);
    assert_eq!(
        st(false, Open, false, true),
        Disabled("Sign in to review this pull request")
    );
    assert_eq!(
        st(false, Open, false, false),
        Disabled(REVIEW_NEEDS_SIGN_IN)
    );
    assert_eq!(
        st(false, Open, true, false),
        Disabled("Git 2.31 or newer is needed to review (install it and restart)")
    );
    assert_eq!(st(false, Open, true, true), Enabled);
    for closed in [Merged, Declined, Superseded] {
        assert_eq!(
            st(false, closed.clone(), true, true),
            Disabled("Only open pull requests can be reviewed here.")
        );
        assert_eq!(st(false, closed, false, false), Disabled(REVIEW_NEEDS_OPEN));
    }
}
