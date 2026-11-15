use super::*;

fn argv(a: &[&str]) -> Vec<String> {
    std::iter::once("nit")
        .chain(a.iter().copied())
        .map(String::from)
        .collect()
}

#[test]
fn a_failed_start_names_the_error_and_the_gpu_overrides() {
    let log = std::path::Path::new("data").join("nit").join("crash.log");
    let t = start_failure_text("no suitable adapter", Some(&log));
    assert!(t.contains("no suitable adapter"), "{t}");
    assert!(
        t.contains("WGPU_BACKEND=gl") && t.contains("WGPU_POWER_PREF=low"),
        "{t}"
    );
    // the real path of the log, not a Windows-only variable
    assert!(t.contains(&log.display().to_string()), "{t}");
    assert!(!t.contains("LOCALAPPDATA"), "{t}");
    assert!(start_failure_text("x", None).contains("crash.log"));
}

#[test]
fn shot_and_bench_are_refused_without_demo() {
    assert!(validate_args(&argv(&["--shot"])).is_err());
    assert!(validate_args(&argv(&["--shot", "out.png"])).is_err());
    assert!(validate_args(&argv(&["--bench"])).is_err());
}

#[test]
fn shot_without_a_path_is_refused_even_with_demo() {
    assert!(validate_args(&argv(&["--demo", "--shot"])).is_err());
    assert!(validate_args(&argv(&["--demo", "--shot", "--screen", "login"])).is_err());
}

#[test]
fn unknown_flags_and_flag_value_forms_are_refused() {
    for bad in [
        vec!["--dmeo"],
        vec!["--shot=out.png"],
        vec!["--demo", "--shot=out.png"],
        vec!["--demo", "--screen=login", "--shot", "out.png"],
        vec!["--demo", "--verbose"],
    ] {
        let err = validate_args(&argv(&bad)).unwrap_err();
        assert!(err.contains("usage:"), "{bad:?}: {err}");
    }
}

#[test]
fn known_flags_in_any_order_pass() {
    let m = validate_args(&argv(&["--screen", "stacked", "--shot", "a.png", "--demo"])).unwrap();
    assert_eq!(m.shot_path.as_deref(), Some("a.png"));
    assert!(validate_args(&argv(&["--demo"])).unwrap().demo);
}

#[test]
fn valid_demo_invocations_pass() {
    let m = validate_args(&argv(&["--demo", "--shot", "out.png"])).unwrap();
    assert_eq!(m.shot_path.as_deref(), Some("out.png"));
    assert!(m.demo && !m.bench);
    assert!(validate_args(&argv(&["--demo", "--bench"])).unwrap().bench);
    let m = validate_args(&argv(&["--demo", "--bench-review"])).unwrap();
    assert!(m.bench && m.bench_review);
    assert!(validate_args(&argv(&["--bench-review"])).is_err());
    let real = validate_args(&argv(&[])).unwrap();
    assert!(!real.demo && real.shot_path.is_none() && real.shots.is_none());
}

#[test]
fn check_token_flags_pass_and_stay_apart_from_the_others() {
    let m = validate_args(&argv(&["--check-token"])).unwrap();
    assert!(m.check.is_some() && !m.demo);
    let m = validate_args(&argv(&["--check-token", "--repo", "acme/api", "--pr", "7"]))
        .unwrap()
        .check
        .unwrap();
    assert_eq!(
        (m.repo.unwrap().full_name(), m.pr),
        ("acme/api".into(), Some(7))
    );
    for bad in [
        vec!["--repo", "acme/api"],
        vec!["--demo", "--pr", "7"],
        vec!["--check-token", "--demo"],
        vec!["--check-token", "--shot", "a.png"],
        vec![
            "--check-token",
            "--write-test",
            "--repo",
            "acme/api",
            "--pr",
            "7",
        ],
        vec!["--check-token", "--token", "x"],
    ] {
        assert!(validate_args(&argv(&bad)).is_err(), "{bad:?}");
    }
    // the old flags are unaffected
    assert!(validate_args(&argv(&["--demo"])).unwrap().check.is_none());
    assert!(validate_args(&argv(&[])).unwrap().check.is_none());
}

#[test]
fn every_scene_name_is_accepted_by_screen() {
    for n in shots::scene_names() {
        let m = validate_args(&argv(&["--demo", "--shot", "a.png", "--screen", n]));
        assert!(m.is_ok(), "{n}");
    }
    let err = validate_args(&argv(&["--demo", "--shot", "a.png", "--screen", "nope"])).unwrap_err();
    assert!(err.contains("nope") && err.contains("review-tree"), "{err}");
}

#[test]
fn shots_defaults_to_the_full_scene_list() {
    let m = validate_args(&argv(&["--demo", "--shots", "out"])).unwrap();
    let s = m.shots.unwrap();
    assert_eq!(s.dir, "out");
    assert_eq!(s.screens, shots::scene_names());
    assert_eq!(
        (s.width, s.height),
        (shots::DEFAULT_WIDTH, shots::DEFAULT_HEIGHT)
    );
}

#[test]
fn shots_needs_demo_and_excludes_shot_and_bench() {
    assert!(validate_args(&argv(&["--shots", "out"])).is_err());
    for other in [
        vec!["--shot", "a.png"],
        vec!["--bench"],
        vec!["--bench-review"],
        vec!["--screen", "login"],
    ] {
        let mut a = vec!["--demo", "--shots", "out"];
        a.extend(other.iter());
        assert!(validate_args(&argv(&a)).is_err(), "{a:?}");
    }
    assert!(validate_args(&argv(&["--demo", "--shots"])).is_err());
    assert!(validate_args(&argv(&["--demo", "--shots", "--screens", "login"])).is_err());
}

#[test]
fn shots_options_need_shots() {
    for f in [
        ["--screens", "login"],
        ["--width", "900"],
        ["--height", "700"],
    ] {
        assert!(
            validate_args(&argv(&["--demo", f[0], f[1]])).is_err(),
            "{f:?}"
        );
    }
}

#[test]
fn shots_screens_are_trimmed_and_validated() {
    let m = validate_args(&argv(&[
        "--demo",
        "--shots",
        "o",
        "--screens",
        " login, inbox ",
    ]))
    .unwrap();
    assert_eq!(m.shots.unwrap().screens, vec!["login", "inbox"]);
    for bad in ["", "login,,inbox", "login,", "nope"] {
        let r = validate_args(&argv(&["--demo", "--shots", "o", "--screens", bad]));
        assert!(r.is_err(), "{bad:?}");
    }
    let err = validate_args(&argv(&["--demo", "--shots", "o", "--screens", "nope"])).unwrap_err();
    assert!(err.contains("\"nope\"") && err.contains("review-conversation-narrow"));
}

#[test]
fn shots_size_flags_are_validated() {
    let m = validate_args(&argv(&[
        "--demo", "--shots", "o", "--width", "1000", "--height", "700",
    ]))
    .unwrap();
    let s = m.shots.unwrap();
    assert_eq!((s.width, s.height), (1000, 700));
    for bad in [["--width", "abc"], ["--height", "0"], ["--width", "--demo"]] {
        let r = validate_args(&argv(&["--demo", "--shots", "o", bad[0], bad[1]]));
        assert!(r.is_err(), "{bad:?}");
    }
}
