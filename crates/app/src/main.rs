//! nit: Bitbucket pull request review. `--demo` runs on built-in data with no network, secrets or store;
//! `--shot out.png [--screen <scene>]` (demo only) saves a screenshot and exits;
//! `--shots <dir> [--screens a,b,c] [--width N --height N]` (demo only) renders every demo scene
//! (or the listed ones) into `<dir>/<scene>.png` in one window; the scenes are listed in `shots.rs`;
//! `--bench` (demo only) prints the average `ui()` time over 300 frames and exits;
//! `--check-token [--account <id|host>] [--repo <w/r>] [--pr <id>]` runs the token self-test
//! (`checktoken.rs`) and exits.

// Release builds are a GUI-subsystem exe: no console window. CLI output (--bench, flag errors)
// is only visible from debug builds (`cargo run -p app`).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod accounts;
mod backend;
mod brand;
mod checktoken;
mod crashlog;
mod demo;
mod gpu;
mod inbox;
mod list;
mod lock;
mod open;
mod repo_picker;
mod review;
mod settings_ui;
mod shots;
mod single_instance;
mod theme;
mod ui;
mod welcome;

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use eframe::egui;
use shots::ShotsArgs;
use store::CredentialManagerSecrets;
use ui::{App, Screen};

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn usage() -> String {
    format!(
        "usage: nit [--demo [--shot <file.png> [--screen <name>]] [--bench | --bench-review]]
       nit --demo --shots <dir> [--screens a,b,c] [--width N --height N]
       nit --check-token [--account <id|host>] [--repo <workspace>/<repo>] [--pr <id>]
       nit --check-token --write-test --repo <workspace>/<repo> --pr <id> --i-understand-this-posts
       nit --version
  screens: {}",
        shots::scene_names().join(", ")
    )
}

/// Smallest supported window (inner size).
const MIN_SIZE: [f32; 2] = [900.0, 600.0];

#[derive(Debug, PartialEq, Eq)]
struct Mode {
    demo: bool,
    bench: bool,
    /// `--bench-review`: the bench runs on the review screen (implies `bench`).
    bench_review: bool,
    shot_path: Option<String>,
    /// `--shots`: render every demo scene in one window.
    shots: Option<ShotsArgs>,
    /// `--check-token`: run the token self-test instead of the app.
    check: Option<checktoken::CheckArgs>,
}

/// `--shots` options: a pixel size flag's value.
fn pixel_flag(args: &[String], flag: &str, default: u32) -> Result<u32, String> {
    if !args.iter().any(|a| a == flag) {
        return Ok(default);
    }
    arg_value(args, flag)
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| (200..=8000).contains(n))
        .ok_or_else(|| format!("{flag} needs a number of pixels between 200 and 8000"))
}

fn shots_args(args: &[String]) -> Result<ShotsArgs, String> {
    let dir = match arg_value(args, "--shots") {
        Some(d) if !d.is_empty() && !d.starts_with("--") => d,
        _ => return Err("--shots needs an output directory".into()),
    };
    let screens = match arg_value(args, "--screens") {
        Some(list) => shots::parse_screens(&list)?,
        None => shots::scene_names(),
    };
    Ok(ShotsArgs {
        dir,
        screens,
        width: pixel_flag(args, "--width", shots::DEFAULT_WIDTH)?,
        height: pixel_flag(args, "--height", shots::DEFAULT_HEIGHT)?,
    })
}

/// `args` excludes nothing: the full argv. `--shot`, `--shots` and `--bench` never touch the
/// network, so they are refused unless `--demo` is present (checked on the flag, not its value).
fn validate_args(args: &[String]) -> Result<Mode, String> {
    // Anything unexpected must not silently fall through to real mode.
    let mut rest = args.iter().skip(1);
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--demo" | "--bench" | "--bench-review" => {}
            "--check-token" | "--write-test" | "--i-understand-this-posts" => {}
            "--shot" | "--screen" | "--shots" | "--screens" | "--width" | "--height" | "--repo"
            | "--pr" | "--account" => {
                rest.next();
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown option {other}\n{}", usage()));
            }
            _ => {}
        }
    }
    if let Some(check) = checktoken::parse_args(args)? {
        return Ok(Mode {
            demo: false,
            bench: false,
            bench_review: false,
            shot_path: None,
            shots: None,
            check: Some(check),
        });
    }
    let has = |f: &str| args.iter().any(|a| a == f);
    let bench_review = has("--bench-review");
    let (demo, bench, shot) = (has("--demo"), has("--bench") || bench_review, has("--shot"));
    let shots = has("--shots");
    if (shot || bench || shots) && !demo {
        return Err(
            "--shot, --shots, --bench and --bench-review need --demo (they never touch the network)"
                .into(),
        );
    }
    if shots && (shot || bench) {
        return Err("--shots cannot be combined with --shot, --bench or --bench-review".into());
    }
    if shots && has("--screen") {
        return Err("--shots takes --screens a,b,c, not --screen".into());
    }
    if !shots {
        if let Some(f) = ["--screens", "--width", "--height"]
            .into_iter()
            .find(|f| has(f))
        {
            return Err(format!("{f} needs --shots"));
        }
    }
    let shot_path = if shot {
        match arg_value(args, "--shot") {
            Some(p) if !p.starts_with("--") => Some(p),
            _ => return Err("--shot needs an output file path".into()),
        }
    } else {
        None
    };
    if let Some(name) = arg_value(args, "--screen") {
        if shots::lookup(&name).is_none() {
            return Err(format!(
                "unknown screen {name:?}; valid screens: {}",
                shots::scene_names().join(", ")
            ));
        }
    }
    let shots = if shots { Some(shots_args(args)?) } else { None };
    Ok(Mode {
        demo,
        bench,
        bench_review,
        shot_path,
        shots,
        check: None,
    })
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 2 && args[1] == "--version" {
        println!("{} {}", brand::NAME, env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let Mode {
        demo,
        bench,
        bench_review,
        shot_path,
        shots,
        check,
    } = match validate_args(&args) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    if let Some(c) = check {
        std::process::exit(checktoken::main(c));
    }
    // two real instances would share one cache file and each post the same draft
    if single_instance::guarded(demo) && !single_instance::check() {
        return Ok(());
    }
    if !demo {
        // Windows: %LOCALAPPDATA%\prview -> nit, before the crash log and the store pick
        // their folder
        brand::migrate_data_dir();
    }
    crashlog::install();
    let screen_name = arg_value(&args, "--screen");
    let shot = shot_path.map(|path| {
        shots::screen_def(screen_name.as_deref())
            .expect("validated by validate_args")
            .shot(path, None)
    });

    // The demo review repository is built here, before the window opens: building it on the
    // UI thread would freeze the window (1000 files take a while). `--shots` builds it once for
    // all its review scenes.
    let review_shot = shot.as_ref().is_some_and(|s| s.screen == Screen::Review);
    let review_shots = shots.as_ref().is_some_and(|a| {
        a.screens
            .iter()
            .any(|n| shots::lookup(n).is_some_and(|d| d.screen == Screen::Review))
    });
    let fixture = (demo && (bench_review || review_shot || review_shots)).then(|| {
        eprintln!("building fixture...");
        Arc::new(if bench_review {
            demo::review_fixture_with(ui::BENCH_REVIEW_FILES)
        } else {
            demo::review_fixture_demo()
        })
    });

    // `--shots` renders in one window and, like `--shot`, must not grab focus or a taskbar
    // slot. The narrow scenes check nothing is clipped at the smallest supported sizes.
    let quiet = shot.is_some() || shots.is_some();
    let size = match (&shots, &shot) {
        (Some(a), _) => shots::ShotsRun::first_size(a),
        (None, Some(_)) => shots::screen_def(screen_name.as_deref())
            .and_then(|d| d.size)
            .unwrap_or((shots::DEFAULT_WIDTH, shots::DEFAULT_HEIGHT)),
        _ => (shots::DEFAULT_WIDTH, shots::DEFAULT_HEIGHT),
    };
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(brand::NAME)
        .with_inner_size([size.0 as f32, size.1 as f32])
        .with_min_inner_size(if shots.is_some() {
            shots::SHOTS_MIN_SIZE
        } else {
            MIN_SIZE
        })
        .with_icon(Arc::new(theme::window_icon()))
        // Wayland app_id and X11 WM_CLASS: matches nit.desktop (StartupWMClass) on Linux
        .with_app_id(brand::NAME);
    if quiet {
        viewport = viewport.with_active(false).with_taskbar(false);
    }
    if shots.is_some() {
        // Verified on Windows 11 (wgpu): a hidden window still renders and screenshots, so a
        // `--shots` run shows nothing at all. `--shot` stays visible (one window, short-lived).
        viewport = viewport.with_visible(false);
    }
    let mut opts = eframe::NativeOptions {
        viewport,
        wgpu_options: gpu::configuration(),
        ..Default::default()
    };
    if bench {
        opts.wgpu_options.surface.present_mode = eframe::egui_wgpu::wgpu::PresentMode::AutoNoVsync;
    }
    if let Some(a) = &shots {
        if let Err(e) = std::fs::create_dir_all(&a.dir) {
            eprintln!("cannot create {}: {e}", a.dir);
            std::process::exit(2);
        }
    }
    let exit = Arc::new(AtomicI32::new(0));
    let exit_code = exit.clone();
    let run = eframe::run_native(
        brand::NAME,
        opts,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            if gpu::record(cc.wgpu_render_state.as_ref()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
                    "{} (software rendering)",
                    brand::NAME
                )));
            }
            ctx.set_theme(egui::ThemePreference::Dark);
            theme::apply(&ctx);
            if let Some(a) = shots {
                return Ok(Box::new(shots::ShotsRun::new(&a, fixture, exit_code)));
            }
            if demo {
                let mut app = if bench_review {
                    App::demo_bench_review()
                } else {
                    App::demo(shot, bench)
                };
                if let Some(f) = fixture {
                    app.preload_review_fixture(f);
                }
                return Ok(Box::new(app));
            }
            // macOS / Unix: a second start asks this window to come to the front
            let raise = ctx.clone();
            single_instance::on_focus_request(move || {
                raise.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                raise.send_viewport_cmd(egui::ViewportCommand::Focus);
                raise.request_repaint();
            });
            let secrets = Arc::new(CredentialManagerSecrets::new(brand::NAME));
            // an install from before the rename: its sign-in moves to the new name once
            let _ = brand::migrate_credentials(
                &*secrets,
                &CredentialManagerSecrets::new(brand::LEGACY_NAME),
            );
            let backend = backend::Backend::open_store().and_then(|store| {
                // the single sign-in of an older build moves to the default account's keys
                // once; on failure the old keys stay and the next start retries
                let _ = accounts::migrate_legacy_bitbucket(&*secrets, &store);
                backend::Backend::new(ctx.clone(), secrets, store)
            });
            Ok(Box::new(App::real(ctx, backend)))
        }),
    );
    if let Err(e) = run {
        // a release build has no console: say it in a box and keep it in crash.log
        let text = start_failure_text(&e.to_string(), crashlog::log_path().as_deref());
        crashlog::record(
            "start-up failed: the window could not be opened",
            &e.to_string(),
        );
        eprintln!("{text}");
        if !quiet {
            single_instance::show_message(&text);
        }
        std::process::exit(1);
    }
    // 0 unless a `--shots` scene failed to save or settle
    std::process::exit(exit.load(Ordering::SeqCst))
}

/// What the user is told when the window cannot be opened (usually the graphics driver);
/// `log` is where the crash log is (`crashlog::log_path`).
fn start_failure_text(error: &str, log: Option<&std::path::Path>) -> String {
    let log = log.map_or_else(|| "crash.log".to_string(), |p| p.display().to_string());
    format!(
        "nit could not open its window.\n\n{}\n\nIf the graphics driver is the cause, start \
         nit with the environment variable WGPU_BACKEND=gl (OpenGL instead of DirectX/Vulkan) \
         or WGPU_POWER_PREF=low (the integrated GPU). Details are in {log}.",
        crashlog::scrub(error)
    )
}

#[cfg(test)]
mod tests {
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
        let m =
            validate_args(&argv(&["--screen", "stacked", "--shot", "a.png", "--demo"])).unwrap();
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
        let err =
            validate_args(&argv(&["--demo", "--shot", "a.png", "--screen", "nope"])).unwrap_err();
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
        let err =
            validate_args(&argv(&["--demo", "--shots", "o", "--screens", "nope"])).unwrap_err();
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
}
